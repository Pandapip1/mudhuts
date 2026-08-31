//! Real-time scheduling settings overridable via
//! `~/.config/mudhuts/config.toml`'s `[performance]` section — same
//! file/mechanism `chrome_config.rs`'s `[chrome]` section, and
//! `theme.rs`'s `[theme]` section use (see [`crate::config::config_path`]).

use crate::config::ConfigFileContents;

/// Clamped to `SCHED_FIFO`'s real `[1, 99]` range (see
/// `rt_sched::apply`), and deliberately conservative rather than
/// maxed out: a `SCHED_FIFO` thread that misbehaves (a bug that spins
/// instead of blocking) can starve the rest of the system, including
/// input handling and anything needed to recover without a hard
/// reset — a low priority still wins the scheduler's attention over
/// every normal (`SCHED_OTHER`) thread on the system without getting
/// anywhere near priorities kernel/IRQ work relies on.
const DEFAULT_SCHED_FIFO_PRIORITY: u8 = 20;

pub struct PerfConfig {
    /// Whether the main thread (the one running the compositor's
    /// event/render loop — see `main.rs`'s `run`) asks the kernel for
    /// `SCHED_FIFO` real-time scheduling instead of the default
    /// `SCHED_OTHER`, to keep frame pacing/input latency more
    /// consistent under system load. **Off by default** — same
    /// "disruptive features default off, explicit opt-in via
    /// `config.toml`" posture this codebase uses for anything with a
    /// real, if unlikely, cost to a live session: a `SCHED_FIFO` thread
    /// that ever spins instead of blocking can hold the CPU indefinitely,
    /// since nothing of lower priority can preempt it. `rt_sched::apply`
    /// also caps `RLIMIT_RTTIME` as a backstop against exactly that (the
    /// kernel kills the process rather than the whole system wedging
    /// solid), but that's a safety net for a bug that shouldn't exist in
    /// the first place, not a reason to treat this as risk-free. Applying
    /// it is also a plain no-op (logged, not fatal — see
    /// `rt_sched::apply`) unless the process actually has `CAP_SYS_NICE`
    /// or a raised `RLIMIT_RTPRIO`, neither of which mudhuts grants
    /// itself; the session/service that launches it (e.g. its systemd
    /// unit) has to grant that separately for this to do anything at all.
    pub sched_fifo: bool,
    /// The `SCHED_FIFO` priority to request when `sched_fifo` is on.
    /// Only meaningful together with `sched_fifo`; not itself gated
    /// on it being on so a user can leave a chosen value in
    /// `config.toml` while toggling `sched_fifo` off and back on.
    pub sched_fifo_priority: u8,
}

impl Default for PerfConfig {
    fn default() -> Self {
        Self { sched_fifo: false, sched_fifo_priority: DEFAULT_SCHED_FIFO_PRIORITY }
    }
}

impl PerfConfig {
    /// Load the default performance config, then apply overrides from
    /// `~/.config/mudhuts/config.toml`'s `[performance]` section if
    /// present — same "any problem is logged and skipped, never fatal"
    /// convention as `ChromeConfig::load`/`Theme::load`.
    /// `config_file` is read once by the caller (`State::new`) and
    /// shared across all `*Config::load()`s — see
    /// `crate::config::read_config_file`'s own doc comment for why
    /// this doesn't read it itself.
    pub(crate) fn load(config_file: &ConfigFileContents) -> Self {
        let mut config = Self::default();
        Self::apply_toml_overrides(&mut config, &config_file.contents, &config_file.source);
        config
    }

    fn apply_toml_overrides(config: &mut PerfConfig, contents: &str, source: &str) {
        let file: ConfigFile = match toml::from_str(contents) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!("failed to parse config at {source}: {err}");
                return;
            }
        };
        let Some(performance) = file.performance else { return };
        if let Some(value) = performance.sched_fifo {
            config.sched_fifo = value;
        }
        if let Some(value) = performance.sched_fifo_priority {
            config.sched_fifo_priority = value;
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    performance: Option<PerformanceToml>,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct PerformanceToml {
    #[serde(default)]
    sched_fifo: Option<bool>,
    #[serde(default)]
    sched_fifo_priority: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_disabled_with_no_config_section() {
        let mut config = PerfConfig::default();
        PerfConfig::apply_toml_overrides(&mut config, "", "test");
        assert!(!config.sched_fifo);
        assert_eq!(config.sched_fifo_priority, DEFAULT_SCHED_FIFO_PRIORITY);
    }

    #[test]
    fn enables_via_the_performance_section() {
        let mut config = PerfConfig::default();
        PerfConfig::apply_toml_overrides(
            &mut config,
            "[performance]\nsched-fifo = true\nsched-fifo-priority = 40\n",
            "test",
        );
        assert!(config.sched_fifo);
        assert_eq!(config.sched_fifo_priority, 40);
    }

    #[test]
    fn malformed_toml_leaves_the_default_in_place() {
        let mut config = PerfConfig::default();
        PerfConfig::apply_toml_overrides(&mut config, "not valid toml [[[", "test");
        assert!(!config.sched_fifo);
        assert_eq!(config.sched_fifo_priority, DEFAULT_SCHED_FIFO_PRIORITY);
    }
}
