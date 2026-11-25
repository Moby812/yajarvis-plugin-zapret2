//! Плагин YaJarvis для управления zapret2 на Яндекс Станции.
//!
//! Плагин умеет **полностью установить** zapret2 (если его ещё нет) и затем
//! включать/выключать обход DPI. Это Rust-пакет (`kind: "rust"`): запускается
//! отдельным процессом с правами службы jarvisd и может исполнять shell-скрипты
//! и ходить в сеть. Lua-плагин тут не подходит — его песочница запрещает ФС,
//! ОС и запуск команд.
//!
//! Установка переиспользует проверенные скрипты из `files/` репозитория
//! (`install.sh`, `start.sh`, `stop.sh`, `uninstall.sh`, `hosts.txt`,
//! `S99zapret2`), которые встроены в бинарник через `include_str!`. Перед
//! запуском они материализуются в рабочий каталог плагина, после чего
//! выполняется `install.sh`. Это гарантирует, что установка из плагина
//! идентична установке вручную по ADB/SSH.
//!
//! stdout полностью занят RPC-протоколом с ядром, поэтому вся диагностика
//! пишется в stderr (она попадает в журнал колонки).

use jarvis_plugin_sdk::{Plugin, Response, Value, json, process};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::process::Command;

/// Каталог, куда install.sh ставит zapret2 (переопределяется настройкой `base`).
const DEFAULT_BASE: &str = "/data/opt/zapret2";
/// Ядро YaJarvis запускает Rust-плагины с очищенным окружением (`env_clear()`),
/// поэтому задаём стандартный PATH колонки (включая Entware /opt).
const DEFAULT_PATH: &str =
    "/opt/sbin:/opt/bin:/data/opt/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// Имя панельной настройки-тумблера.
const SETTING_ENABLED: &str = "enabled";
/// Имя процесса демона nfqws2 (для проверки, что обход реально запущен).
const NFPROC: &str = "nfqws2";

// Встроенные скрипты из files/ репозитория. Относительные пути — от crate root.
const INSTALL_SH: &str = include_str!("../../files/install.sh");
const START_SH: &str = include_str!("../../files/start.sh");
const STOP_SH: &str = include_str!("../../files/stop.sh");
const UNINSTALL_SH: &str = include_str!("../../files/uninstall.sh");
const HOSTS_TXT: &str = include_str!("../../files/hosts.txt");
const S99_ZAPRET2: &str = include_str!("../../files/S99zapret2");

fn effective_path() -> String {
    std::env::var("PATH")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PATH.to_string())
}

fn parse_base(settings: &serde_json::Map<String, Value>) -> PathBuf {
    settings
        .get("base")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BASE))
}

struct State {
    enabled: bool,
    base: PathBuf,
}

#[derive(Clone)]
struct Zapret {
    state: Arc<Mutex<State>>,
    work_dir: PathBuf,
}

impl Zapret {
    fn new(settings: &serde_json::Map<String, Value>) -> Self {
        let enabled = settings
            .get(SETTING_ENABLED)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let base = parse_base(settings);
        // Рабочий каталог процесса (данные плагина). Сюда кладём временные
        // скрипты установки.
        let work_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
        Zapret {
            state: Arc::new(Mutex::new(State { enabled, base })),
            work_dir,
        }
    }

    fn enabled(&self) -> bool {
        self.state.lock().unwrap().enabled
    }

    fn base(&self) -> PathBuf {
        self.state.lock().unwrap().base.clone()
    }

    fn set_enabled(&self, v: bool) {
        let mut st = self.state.lock().unwrap();
        st.enabled = v;
    }

    fn is_installed(&self) -> bool {
        self.base().join("start.sh").is_file()
    }

    /// Материализует встроенные скрипты в подкаталог работы плагина.
    fn materialize_scripts(&self) -> Result<PathBuf, String> {
        let dir = self.work_dir.join("zapret-files");
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("не удалось создать {}: {}", dir.display(), e))?;
        let files: &[(&str, &str)] = &[
            ("install.sh", INSTALL_SH),
            ("start.sh", START_SH),
            ("stop.sh", STOP_SH),
            ("uninstall.sh", UNINSTALL_SH),
            ("hosts.txt", HOSTS_TXT),
            ("S99zapret2", S99_ZAPRET2),
        ];
        for (name, content) in files {
            std::fs::write(dir.join(name), content)
                .map_err(|e| format!("не удалось записать {}: {}", name, e))?;
        }
        Ok(dir)
    }

    /// Есть ли живой процесс nfqws2.
    ///
    /// BusyBox на Станции может не иметь `pgrep`, поэтому пробуем по очереди
    /// `pgrep -x`, `pidof`, а в конце — сверить PID из pid-файла с /proc.
    async fn is_running(&self) -> bool {
        let path = effective_path();
        // 1) pgrep
        let out = Command::new("/bin/sh")
            .args(["-c", "pgrep -x \"$1\" >/dev/null 2>&1 || pidof \"$1\" >/dev/null 2>&1", "--", NFPROC])
            .env("PATH", &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .await;
        if let Ok(o) = out {
            if o.status.success() {
                return true;
            }
        }
        // 2) PID из pid-файла, живой ли он в /proc.
        let pid_file = self.base().join("nfqws2.pid");
        if let Ok(pid) = std::fs::read_to_string(&pid_file) {
            let pid = pid.trim();
            if let Ok(comm) = std::fs::read_to_string(format!("/proc/{}/comm", pid)) {
                if comm.trim() == NFPROC {
                    return true;
                }
            }
        }
        false
    }

    async fn run_script_with_env(
        &self,
        script: &Path,
        env: &[(&str, &str)],
    ) -> Result<String, String> {
        let path = effective_path();
        let out = Command::new("/bin/sh")
            .arg(script)
            .current_dir(&self.work_dir)
            .env("PATH", &path)
            .envs(env.iter().copied())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| format!("не удалось запустить {}: {}", script.display(), e))?;
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        if out.status.success() {
            Ok(if detail.is_empty() {
                "OK".to_string()
            } else {
                format!("OK: {detail}")
            })
        } else {
            let mut msg = format!("ошибка (exit {})", out.status.code().unwrap_or(-1));
            if !detail.is_empty() {
                msg.push_str(": ");
                msg.push_str(&detail);
            }
            Err(msg)
        }
    }

    /// Полная установка zapret2, если его ещё нет.
    async fn ensure_installed(&self) -> Result<String, String> {
        if self.is_installed() {
            return Ok("zapret2 уже установлен".to_string());
        }
        let src = self.materialize_scripts()?;
        eprintln!("[zapret] установка из {}", src.display());
        let install_script = src.join("install.sh");
        let base = self.base().display().to_string();
        // install.sh сам читает config-файлы из каталога, где он лежит.
        self.run_script_with_env(&install_script, &[("BASE", &base)])
            .await
    }

    /// Удаление zapret2 (используется из инструмента "zapret_uninstall").
    async fn uninstall(&self) -> Result<String, String> {
        let base_dir = self.base();
        let uninstall_script = base_dir.join("uninstall.sh");
        if !uninstall_script.is_file() {
            return Err("uninstall.sh не найден (zapret2 уже удалён?)".to_string());
        }
        let base = base_dir.display().to_string();
        let result = self
            .run_script_with_env(&uninstall_script, &[("BASE", &base)])
            .await?;
        self.set_enabled(false);
        Ok(result)
    }

    /// Приводит zapret к требуемому состоянию `want` (включить/выключить).
    /// Сначала убеждается, что zapret установлен.
    async fn set_running(&self, want: bool) -> Result<String, String> {
        self.set_enabled(want);
        if want {
            self.ensure_installed().await?;
        }
        let now = self.is_running().await;
        if now == want {
            return Ok(if want {
                "zapret2 уже включён".to_string()
            } else {
                "zapret2 уже выключен".to_string()
            });
        }
        let base_dir = self.base();
        let script = base_dir.join(if want { "start.sh" } else { "stop.sh" });
        if !script.is_file() {
            return Err(format!("не найден скрипт {}", script.display()));
        }
        let base = base_dir.display().to_string();
        self.run_script_with_env(&script, &[("BASE", &base)]).await?;
        Ok(if want {
            "zapret2 включён".to_string()
        } else {
            "zapret2 выключен".to_string()
        })
    }

    /// Обрабатывает голосовую/быструю команду. Null — не наша фраза.
    /// У ядра таймаут на `command` равен 2 с, поэтому первичную установку запускаем в фоне.
    async fn handle_command(&self, text: &str) -> Value {
        let t = text.trim();
        match t {
            "включи запрет" | "включи обход" | "включи обход блокировок" | "запет включи"
            | "запрет включи" => {
                if !self.is_installed() {
                    let this = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = this.set_running(true).await {
                            eprintln!("[zapret] фоновая установка не удалась: {e}");
                        }
                    });
                    return Value::String(
                        "Устанавливаю и включаю zapret2 в фоне, подождите около минуты".to_string(),
                    );
                }
                Value::String(self.set_running(true).await.unwrap_or_else(err_msg))
            }
            "выключи запрет" | "выключи обход" | "выключи обход блокировок" | "запет выключи"
            | "запрет выключи" => {
                Value::String(self.set_running(false).await.unwrap_or_else(err_msg))
            }
            _ => Value::Null,
        }
    }

    async fn handle_tool(&self, name: &str, params: &Value) -> Value {
        match name {
            "set_zapret" => {
                let enabled = params["enabled"].as_bool().unwrap_or(false);
                if enabled && !self.is_installed() {
                    let this = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = this.set_running(true).await {
                            eprintln!("[zapret] фоновая установка не удалась: {e}");
                        }
                    });
                    return Value::String(
                        "Начал установку и запуск zapret2 в фоне, это займёт около минуты".to_string(),
                    );
                }
                Value::String(self.set_running(enabled).await.unwrap_or_else(err_msg))
            }
            "zapret_status" => Value::String(if self.is_running().await {
                format!(
                    "zapret2 установлен и обход включён (каталог {})",
                    self.base().display()
                )
            } else {
                "zapret2 не установлен или обход выключен".to_string()
            }),
            "zapret_uninstall" => Value::String(self.uninstall().await.unwrap_or_else(err_msg)),
            _ => Value::Null,
        }
    }
}

fn err_msg(e: String) -> String {
    e
}

impl Plugin for Zapret {
    fn configure(&self, settings: serde_json::Map<String, Value>) {
        let want = settings
            .get(SETTING_ENABLED)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let new_base = parse_base(&settings);
        {
            let mut st = self.state.lock().unwrap();
            st.base = new_base;
        }
        eprintln!("[zapret] панель изменила enabled -> {}", want);
        // Панельный тумблер. Синхронизируем zapret асинхронно в фоне.
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = this.set_running(want).await {
                eprintln!("[zapret] не удалось применить настройку панели: {}", e);
            }
        });
    }

    fn tools(&self) -> Vec<Value> {
        // Ядро YaJarvis ожидает плоскую схему инструмента: type/name/description/parameters на верхнем уровне.
        vec![
            json!({
                "type": "function",
                "name": "set_zapret",
                "description": "Включить или выключить обход DPI (zapret2) на этой Яндекс Станции. При включении автоматически установит zapret2, если его ещё нет. Вызывается по просьбе пользователя.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "enabled": {
                            "type": "boolean",
                            "description": "true — включить обход, false — выключить"
                        }
                    },
                    "required": ["enabled"]
                }
            }),
            json!({
                "type": "function",
                "name": "zapret_status",
                "description": "Сообщает, установлен ли zapret2 и включён ли обход DPI на Станции.",
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }),
            json!({
                "type": "function",
                "name": "zapret_uninstall",
                "description": "Полностью удаляет zapret2 со Станции (останавливает обход и убирает автозапуск).",
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }),
        ]
    }

    fn request(&self, method: &str, params: Value) -> Response<'_> {
        let this = self.clone();
        let method = method.to_owned();
        Box::pin(async move {
            match method.as_str() {
                "command" => Ok(this
                    .handle_command(params.as_str().unwrap_or(""))
                    .await),
                "tool" => Ok(this
                    .handle_tool(params["name"].as_str().unwrap_or(""), &params["arguments"])
                    .await),
                "status" => Ok(Value::String(if this.is_running().await {
                    "обход включён".to_string()
                } else {
                    "обход выключен".to_string()
                })),
                // При остановке плагина zapret2 не трогаем — пользователь мог
                // включить его намеренно, и выключаться он не должен.
                "stop" | "shutdown" => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    process::serve(|init| {
        let plugin = Zapret::new(&init.settings);
        // Применяем панельную настройку при старте процесса: приводим zapret
        // к состоянию `enabled` из панели. Выполняем в фоне, не блокируя
        // рукопожатие с ядром.
        let boot = plugin.clone();
        let want = plugin.enabled();
        tokio::spawn(async move {
            if let Err(e) = boot.set_running(want).await {
                eprintln!("[zapret] стартовая синхронизация: {}", e);
            }
        });
        Ok(Box::new(plugin))
    })
    .await
}