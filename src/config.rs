//! User configuration (`config.toml`). Every key is optional; anything left
//! out falls back to the defaults below, which mirror `DEFAULT_TOML`.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Discord application ID. Its name is what Discord shows as "Playing …".
    pub client_id: String,
    /// 0 = Playing, 2 = Listening to, 3 = Watching, 5 = Competing in.
    pub activity_type: u8,
    /// What the member list shows next to your name: "name" (app name),
    /// "state" or "details".
    pub status_display: String,
    /// Show Discord's elapsed-time counter, starting when the session started.
    pub show_elapsed: bool,
    /// Seconds without hooks (and a quiet transcript) before an idle session
    /// is dropped and the card cleared. 0 = keep until SessionEnd. Clamped to
    /// `IDLE_TIMEOUT_MIN..=IDLE_TIMEOUT_MAX` on load.
    pub idle_timeout: u64,
    /// Seconds between rotation frames (idle stats carousel). Clamped to
    /// `ROTATION_INTERVAL_MIN..=ROTATION_INTERVAL_MAX` on load.
    pub rotation_interval: u64,
    /// Add a "View on GitHub" button when the project has a github.com origin.
    /// Off by default: private repos would leak their URL.
    pub github_button: bool,
    /// Projects (directory names or path prefixes) to anonymize on the card.
    pub hidden_projects: Vec<String>,
    /// What `{project}` renders as for hidden projects.
    pub hidden_project_name: String,
    /// Import lifetime stats from existing Claude Code transcripts at startup.
    pub scan_history: bool,
    /// Seconds between background rescans of ~/.claude/projects. 0 = never.
    pub rescan_interval: u64,
    /// Up to two buttons, `{ label = "...", url = "https://..." }`.
    pub buttons: Vec<Button>,
    pub assets: Assets,
    pub status: StatusTemplates,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Button {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Assets {
    /// Tooltip of the large image (templated).
    pub large_text: String,
    /// Optional small image key/URL and tooltip (templated).
    pub small_image: String,
    pub small_text: String,
    pub working: String,
    pub thinking: String,
    pub compacting: String,
    pub notification: String,
    pub idle: String,
}

/// Each `[status.*]` table is merged key by key onto that status's built-in
/// template, so a partial table keeps the remaining defaults.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(from = "RawStatusTemplates")]
pub struct StatusTemplates {
    pub working: Template,
    pub thinking: Template,
    pub compacting: Template,
    pub notification: Template,
    pub idle: Template,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawStatusTemplates {
    working: RawTemplate,
    thinking: RawTemplate,
    compacting: RawTemplate,
    notification: RawTemplate,
    idle: RawTemplate,
}

/// `None` = key absent; an explicit `""` or `rotation = []` is kept.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTemplate {
    details: Option<String>,
    state: Option<String>,
    rotation: Option<Vec<Frame>>,
}

impl RawTemplate {
    fn merge_onto(self, mut t: Template) -> Template {
        if let Some(v) = self.details {
            t.details = v;
        }
        if let Some(v) = self.state {
            t.state = v;
        }
        if let Some(v) = self.rotation {
            t.rotation = v;
        }
        t
    }
}

impl From<RawStatusTemplates> for StatusTemplates {
    fn from(r: RawStatusTemplates) -> Self {
        let d = StatusTemplates::default();
        StatusTemplates {
            working: r.working.merge_onto(d.working),
            thinking: r.thinking.merge_onto(d.thinking),
            compacting: r.compacting.merge_onto(d.compacting),
            notification: r.notification.merge_onto(d.notification),
            idle: r.idle.merge_onto(d.idle),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub details: String,
    pub state: String,
    /// Extra frames cycled after the base frame. A frame is skipped while any
    /// variable it uses is empty or zero.
    pub rotation: Vec<Frame>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(default)]
pub struct Frame {
    pub details: String,
    pub state: String,
}

fn t(details: &str, state: &str) -> Template {
    Template { details: details.into(), state: state.into(), rotation: Vec::new() }
}

fn f(details: &str, state: &str) -> Frame {
    Frame { details: details.into(), state: state.into() }
}

/// Shorter idle timeouts would clear and re-set the card between ordinary
/// prompts, burning Discord's rate-limit budget.
pub const IDLE_TIMEOUT_MIN: u64 = 60;
/// A week; use 0 to keep sessions forever.
pub const IDLE_TIMEOUT_MAX: u64 = 7 * 86_400;
/// Discord accepts an activity update at most every 4 s.
pub const ROTATION_INTERVAL_MIN: u64 = 5;
pub const ROTATION_INTERVAL_MAX: u64 = 86_400;

const CDN: &str = "https://cdn.qualit.ly";

impl Default for Assets {
    fn default() -> Self {
        Assets {
            large_text: "{model} · {total_time} on Claude".into(),
            small_image: String::new(),
            small_text: String::new(),
            working: format!("{CDN}/clawd-working-building.gif"),
            thinking: format!("{CDN}/clawd-working-typing.gif"),
            compacting: format!("{CDN}/clawd-working-typing.gif"),
            notification: format!("{CDN}/clawd-notification.gif"),
            idle: format!("{CDN}/clawd-sleeping.gif"),
        }
    }
}

impl Default for StatusTemplates {
    fn default() -> Self {
        let mut idle = t("Idle in {project}", "{model} · {today_time} today");
        idle.rotation = vec![
            f("Today · {today_time}", "{today_prompts} prompts · {today_tokens} tokens"),
            f("{total_time} on Claude", "{total_sessions} sessions · {total_prompts} prompts"),
            f("Lifetime · {total_tokens} tokens", "{streak} day streak"),
        ];
        StatusTemplates {
            working: t("Working in {project}", "{tool} · {file} · {tokens} tokens"),
            thinking: t("Thinking in {project}", "{model} · {prompts} prompts · {tokens} tokens"),
            compacting: t("Compacting context in {project}", "{model} · {tokens} tokens"),
            notification: t("Waiting on you · {project}", "{model} · {prompts} prompts"),
            idle,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            client_id: "1506443909406920948".into(),
            activity_type: 0,
            status_display: "name".into(),
            show_elapsed: true,
            idle_timeout: 900,
            rotation_interval: 15,
            github_button: false,
            hidden_projects: Vec::new(),
            hidden_project_name: "a private project".into(),
            scan_history: true,
            rescan_interval: 1800,
            buttons: Vec::new(),
            assets: Assets::default(),
            status: StatusTemplates::default(),
        }
    }
}

/// `*v` clamped to `min..=max`, noting in `notes` when it had to be changed.
fn clamp(notes: &mut Vec<String>, key: &str, v: &mut u64, min: u64, max: u64) {
    let c = (*v).clamp(min, max);
    if c != *v {
        let bound = if *v < min { "below the minimum" } else { "above the maximum" };
        notes.push(format!("{key} = {v} is {bound}; using {c}"));
        *v = c;
    }
}

impl Config {
    /// Load from `path`; a missing file means defaults, a broken one is
    /// reported and also falls back to defaults so presence keeps working.
    pub fn load(path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(c) => c.sanitized(),
                Err(e) => {
                    crate::error!("{}: {e}; using defaults", path.display());
                    Config::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                crate::error!("{}: {e}; using defaults", path.display());
                Config::default()
            }
        }
    }

    /// Clamp numeric settings into ranges the daemon's millisecond
    /// arithmetic (`secs as i64 * 1000`) can't overflow.
    pub fn sanitized(mut self) -> Config {
        for note in self.clamp() {
            crate::warn!("{note}");
        }
        self
    }

    /// `config.toml` text as `load` reads it (sanitized), without logging:
    /// the clamp warnings come back instead, for `claude-presence tui`,
    /// whose screen stderr output would garble.
    pub fn parse(s: &str) -> Result<(Config, Vec<String>), toml::de::Error> {
        let mut c: Config = toml::from_str(s)?;
        let notes = c.clamp();
        Ok((c, notes))
    }

    /// The `sanitized` clamps; what had to change, as warnings.
    fn clamp(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.idle_timeout != 0 {
            clamp(&mut notes, "idle_timeout", &mut self.idle_timeout, IDLE_TIMEOUT_MIN, IDLE_TIMEOUT_MAX);
        }
        let r = &mut self.rotation_interval;
        clamp(&mut notes, "rotation_interval", r, ROTATION_INTERVAL_MIN, ROTATION_INTERVAL_MAX);
        notes
    }

    pub fn status_display_type(&self) -> Option<u8> {
        match self.status_display.as_str() {
            "state" => Some(1),
            "details" => Some(2),
            _ => None,
        }
    }
}

/// Written by `claude-presence install` when no config exists yet.
pub const DEFAULT_TOML: &str = r#"# claude-presence configuration. Every key is optional — delete what you
# don't care about and the built-in default (shown here) is used.
#
# Template variables (rotation frames are skipped while any of their
# variables is empty or zero):
#   session:  {project} {branch} {model} {tool} {file} {tokens} {tokens_in}
#             {tokens_out} {prompts} {tools} {session_time} {status}
#   today:    {today_time} {today_tokens} {today_prompts}
#   lifetime: {total_time} {total_tokens} {total_sessions} {total_prompts}
#             {streak}
# Empty "·"-separated segments are collapsed automatically.

# Discord application ID (https://discord.com/developers/applications).
# Its name is what Discord shows as "Playing <name>".
client_id = "1506443909406920948"

# 0 = Playing, 2 = Listening to, 3 = Watching, 5 = Competing in
activity_type = 0

# What the member list shows next to your name: "name", "state" or "details".
status_display = "name"

show_elapsed = true

# Seconds without activity before an idle session's card is cleared.
# (A closed Claude Code clears it immediately via the SessionEnd hook.)
# 0 = keep until SessionEnd; otherwise clamped to 60..604800 (7 days).
idle_timeout = 900

# Seconds between rotation frames; clamped to 5..86400 (1 day).
rotation_interval = 15

# Adds a "View on GitHub" button for github.com origins. Off by default
# because it would publish private repository URLs too.
github_button = false

# Directory names or path prefixes shown as `hidden_project_name` instead.
hidden_projects = []
hidden_project_name = "a private project"

# Import lifetime stats from existing transcripts in ~/.claude/projects.
scan_history = true
# Seconds between background rescans (picks up subagent transcripts etc.).
rescan_interval = 1800

# Up to two buttons.
buttons = []
# buttons = [{ label = "My website", url = "https://example.com" }]

[assets]
large_text = "{model} · {total_time} on Claude"
small_image = ""
small_text = ""
working = "https://cdn.qualit.ly/clawd-working-building.gif"
thinking = "https://cdn.qualit.ly/clawd-working-typing.gif"
compacting = "https://cdn.qualit.ly/clawd-working-typing.gif"
notification = "https://cdn.qualit.ly/clawd-notification.gif"
idle = "https://cdn.qualit.ly/clawd-sleeping.gif"

[status.working]
details = "Working in {project}"
state = "{tool} · {file} · {tokens} tokens"

[status.thinking]
details = "Thinking in {project}"
state = "{model} · {prompts} prompts · {tokens} tokens"

[status.compacting]
details = "Compacting context in {project}"
state = "{model} · {tokens} tokens"

[status.notification]
details = "Waiting on you · {project}"
state = "{model} · {prompts} prompts"

[status.idle]
details = "Idle in {project}"
state = "{model} · {today_time} today"
rotation = [
  { details = "Today · {today_time}", state = "{today_prompts} prompts · {today_tokens} tokens" },
  { details = "{total_time} on Claude", state = "{total_sessions} sessions · {total_prompts} prompts" },
  { details = "Lifetime · {total_tokens} tokens", state = "{streak} day streak" },
]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_toml_matches_defaults() {
        let parsed: Config = toml::from_str(DEFAULT_TOML).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn partial_config_keeps_defaults() {
        let c: Config = toml::from_str("client_id = \"42\"\n[status.idle]\ndetails = \"zzz\"").unwrap();
        assert_eq!(c.client_id, "42");
        assert_eq!(c.status.idle.details, "zzz");
        assert_eq!(c.status.working, Config::default().status.working);
        assert_eq!(c.assets, Assets::default());
    }

    #[test]
    fn partial_status_table_keeps_that_status_defaults() {
        let d = Config::default();
        let c: Config = toml::from_str("[status.idle]\ndetails = \"zzz\"\n[status.thinking]\nstate = \"s\"").unwrap();
        assert_eq!(c.status.idle.details, "zzz");
        assert_eq!(c.status.idle.state, d.status.idle.state);
        assert_eq!(c.status.idle.rotation, d.status.idle.rotation);
        assert!(!c.status.idle.rotation.is_empty());
        assert_eq!(c.status.thinking.details, d.status.thinking.details);
        assert_eq!(c.status.thinking.state, "s");
        assert_eq!(c.status.thinking.rotation, d.status.thinking.rotation);
        assert_eq!(c.status.working, d.status.working);
        assert_eq!(c.status.compacting, d.status.compacting);
        assert_eq!(c.status.notification, d.status.notification);
        // An empty table changes nothing.
        let c: Config = toml::from_str("[status.idle]\n[status]\n").unwrap();
        assert_eq!(c, d);
    }

    #[test]
    fn explicit_empty_status_values_win() {
        let c: Config = toml::from_str("[status.idle]\ndetails = \"\"\nrotation = []\n").unwrap();
        assert_eq!(c.status.idle.details, "");
        assert_eq!(c.status.idle.state, Config::default().status.idle.state);
        assert!(c.status.idle.rotation.is_empty(), "rotation = [] disables rotation");

        let c: Config = toml::from_str("[status.working]\nrotation = [{ details = \"r\" }]\n").unwrap();
        assert_eq!(c.status.working.details, Config::default().status.working.details);
        assert_eq!(c.status.working.rotation, vec![f("r", "")]);
    }

    #[test]
    fn partial_assets_and_buttons_keep_defaults() {
        let d = Config::default();
        let c: Config = toml::from_str("[assets]\nidle = \"x\"\nlarge_text = \"\"\n").unwrap();
        assert_eq!(c.assets.idle, "x");
        assert_eq!(c.assets.large_text, "", "explicit empty string wins");
        assert_eq!(c.assets.working, d.assets.working);
        assert_eq!(c.assets.notification, d.assets.notification);
        assert_eq!(c.buttons, d.buttons);

        let c: Config = toml::from_str("buttons = [{ label = \"a\", url = \"https://e.com\" }]\n").unwrap();
        assert_eq!(c.buttons, vec![Button { label: "a".into(), url: "https://e.com".into() }]);
        assert_eq!(c.assets, d.assets);
    }

    fn load_str(name: &str, toml: &str) -> Config {
        let p = std::env::temp_dir().join(format!("cp-config-{name}-{}.toml", std::process::id()));
        std::fs::write(&p, toml).unwrap();
        let c = Config::load(&p);
        let _ = std::fs::remove_file(&p);
        c
    }

    #[test]
    fn clamps_durations_on_load() {
        let c = load_str("huge", "idle_timeout = 9223372036854775807\nrotation_interval = 9223372036854775807\n");
        assert_eq!(c.idle_timeout, IDLE_TIMEOUT_MAX);
        assert_eq!(c.rotation_interval, ROTATION_INTERVAL_MAX);
        // The daemon's millisecond arithmetic must not overflow.
        assert!((c.idle_timeout as i64).checked_mul(1000).is_some());
        assert!((c.rotation_interval as i64).checked_mul(1000).is_some());

        let c = load_str("zero", "idle_timeout = 0\nrotation_interval = 0\n");
        assert_eq!(c.idle_timeout, 0, "0 keeps meaning \"never expire\"");
        assert_eq!(c.rotation_interval, ROTATION_INTERVAL_MIN);

        let c = load_str("small", "idle_timeout = 1\nrotation_interval = 1\n");
        assert_eq!(c.idle_timeout, IDLE_TIMEOUT_MIN);
        assert_eq!(c.rotation_interval, ROTATION_INTERVAL_MIN);

        let c = load_str("ok", "idle_timeout = 1200\nrotation_interval = 30\n");
        assert_eq!((c.idle_timeout, c.rotation_interval), (1200, 30));
    }

    #[test]
    fn parse_reports_clamps_instead_of_logging() {
        let (c, notes) = Config::parse("idle_timeout = 1\nrotation_interval = 30\n").unwrap();
        assert_eq!((c.idle_timeout, c.rotation_interval), (IDLE_TIMEOUT_MIN, 30));
        assert_eq!(notes, ["idle_timeout = 1 is below the minimum; using 60"]);
        let (c, notes) = Config::parse("").unwrap();
        assert_eq!(c, Config::default());
        assert!(notes.is_empty());
        assert!(Config::parse("this is not toml").is_err());
        assert!(Config::parse("idle_timeout = \"900\"\n").is_err());
    }

    #[test]
    fn defaults_are_within_clamp_ranges() {
        assert_eq!(Config::default().sanitized(), Config::default());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let c = load_str("unknown", "no_such_key = 1\n[status.idle]\nbogus = \"x\"\ndetails = \"d\"\n[extra]\na = 1\n");
        assert_eq!(c.status.idle.details, "d");
        assert_eq!(c.client_id, Config::default().client_id);
    }

    #[test]
    fn invalid_values_fall_back_to_defaults() {
        for bad in [
            "idle_timeout = -1\n",
            "idle_timeout = \"900\"\n",
            "activity_type = 300\n",
            "buttons = [{ label = \"x\" }]\n",
            "[status.idle]\ndetails = 1\n",
            "[status.idle]\nrotation = \"x\"\n",
            "this is not toml",
        ] {
            assert_eq!(load_str("bad", bad), Config::default(), "{bad:?}");
        }
        assert_eq!(load_str("empty", ""), Config::default());
        assert_eq!(Config::load(Path::new("/nonexistent/cp/config.toml")), Config::default());
    }

    #[test]
    fn status_display_mapping() {
        let mut c = Config::default();
        assert_eq!(c.status_display_type(), None);
        c.status_display = "state".into();
        assert_eq!(c.status_display_type(), Some(1));
        c.status_display = "details".into();
        assert_eq!(c.status_display_type(), Some(2));
        c.status_display = "bogus".into();
        assert_eq!(c.status_display_type(), None);
    }
}
