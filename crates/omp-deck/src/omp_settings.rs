//! Data model of the omp settings editor: the loaded catalog with per-row UI state, cached search
//! haystacks, value parsing for the editors and a few pure formatting helpers.

use std::path::Path;

use deck_core::omp::{SECRET_PLACEHOLDER, Setting, SettingKind, SettingsCatalog};
use serde_json::Value;

use crate::statusbar::Defaults;

/// Row height guess before a row has been laid out once.
pub const ROW_HEIGHT_GUESS: f32 = 76.0;

pub struct Row {
    pub setting: Setting,
    /// Humanized label, computed once.
    pub label: String,
    /// Lowercased `key \n label \n description`, computed once.
    haystack: String,
    /// A write is in flight.
    pub pending: bool,
    /// omp's message from the last failed write.
    pub error: Option<String>,
    /// Local validation message for the text being typed.
    pub input_error: Option<String>,
    /// Text being edited (number / string fields); `None` = show the stored value.
    pub buf: Option<String>,
    /// Value sent with the write in flight (fallback when omp echoes `null`).
    sent: Option<Value>,
    /// Last laid-out height, used to skip off-screen rows.
    pub height: f32,
}

impl Row {
    fn new(setting: Setting) -> Row {
        let label = setting.label();
        let haystack =
            format!("{}\n{}\n{}", setting.key, label, setting.description).to_lowercase();
        Row {
            setting,
            label,
            haystack,
            pending: false,
            error: None,
            input_error: None,
            buf: None,
            sent: None,
            height: ROW_HEIGHT_GUESS,
        }
    }

    /// Stored value, falling back to the default.
    pub fn effective(&self) -> Option<&Value> {
        self.setting
            .value
            .as_ref()
            .or(self.setting.default.as_ref())
    }

    pub fn matches(&self, terms: &[String]) -> bool {
        terms.iter().all(|t| self.haystack.contains(t.as_str()))
    }

    /// Marks a write as started. `sent` is only kept for non-secret settings (a secret must not
    /// outlive the write in UI state).
    pub fn begin(&mut self, sent: Option<Value>) {
        self.pending = true;
        self.error = None;
        self.input_error = None;
        self.sent = if self.setting.secret { None } else { sent };
    }

    /// Applies a write result: success updates the value, failure keeps it and records omp's message.
    pub fn finish(&mut self, outcome: Result<Option<Value>, String>) {
        self.pending = false;
        match outcome {
            Ok(Some(saved)) => {
                self.setting.value = Some(if self.setting.secret {
                    Value::String(SECRET_PLACEHOLDER.to_owned())
                } else if saved.is_null() {
                    self.sent.take().unwrap_or(Value::Null)
                } else {
                    saved
                });
                self.buf = None;
                self.error = None;
            }
            Ok(None) => {
                self.setting.value = None;
                self.buf = None;
                self.error = None;
            }
            Err(e) => {
                self.buf = None;
                self.error = Some(e);
            }
        }
        self.sent = None;
    }
}

pub struct Section {
    pub title: String,
    /// Indices into `TabModel::rows`.
    pub rows: Vec<usize>,
}

pub struct TabModel {
    pub id: String,
    pub label: String,
    pub rows: Vec<Row>,
    /// Rows grouped by `Setting::section()`, in order of first appearance.
    pub sections: Vec<Section>,
}

pub struct Model {
    pub tabs: Vec<TabModel>,
    /// Where omp keeps its config, `~`-abbreviated, for the note banner.
    pub config_dir: String,
}

/// Matching rows of one tab.
pub struct TabHits {
    pub tab: usize,
    pub rows: Vec<usize>,
}

impl Model {
    pub fn from_catalog(catalog: SettingsCatalog, config_dir: String) -> Model {
        let tabs = catalog
            .tabs
            .into_iter()
            .map(|t| {
                let rows: Vec<Row> = t.settings.into_iter().map(Row::new).collect();
                let mut sections: Vec<Section> = Vec::new();
                for (i, row) in rows.iter().enumerate() {
                    let raw = row.setting.section();
                    let title = section_title(raw);
                    match sections.iter_mut().find(|s| s.title == title) {
                        Some(s) => s.rows.push(i),
                        None => sections.push(Section {
                            title,
                            rows: vec![i],
                        }),
                    }
                }
                TabModel {
                    id: t.id,
                    label: t.label,
                    rows,
                    sections,
                }
            })
            .collect();
        Model { tabs, config_dir }
    }

    pub fn find_mut(&mut self, key: &str) -> Option<&mut Row> {
        self.tabs
            .iter_mut()
            .flat_map(|t| t.rows.iter_mut())
            .find(|r| r.setting.key == key)
    }

    /// Rows of every tab matching all whitespace-separated terms of `query`, grouped by tab.
    pub fn search(&self, query: &str) -> Vec<TabHits> {
        let terms = query_terms(query);
        if terms.is_empty() {
            return Vec::new();
        }
        self.tabs
            .iter()
            .enumerate()
            .filter_map(|(tab, t)| {
                let rows: Vec<usize> = t
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.matches(&terms))
                    .map(|(i, _)| i)
                    .collect();
                (!rows.is_empty()).then_some(TabHits { tab, rows })
            })
            .collect()
    }
}

/// Lowercased whitespace-separated terms.
pub fn query_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// `statusLine.preset` → "Status line › Preset"; "" → "General".
pub fn section_title(section: &str) -> String {
    if section.is_empty() {
        return "General".to_owned();
    }
    section
        .split('.')
        .map(|seg| {
            let mut out = String::new();
            let mut prev_lower = false;
            for c in seg.chars() {
                if c.is_uppercase() && prev_lower {
                    out.push(' ');
                }
                prev_lower = c.is_lowercase() || c.is_ascii_digit();
                if out.is_empty() {
                    out.extend(c.to_uppercase());
                } else {
                    out.extend(c.to_lowercase());
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join(" › ")
}

/// Config directory shown in the banner: `PI_CODING_AGENT_DIR` or `~/.omp/agent`, `~`-abbreviated.
pub fn config_dir_display(env: Option<&str>, home: Option<&Path>) -> String {
    let dir = match (env.filter(|e| !e.is_empty()), home) {
        (Some(e), _) => Path::new(e).to_path_buf(),
        (None, Some(h)) => h.join(".omp").join("agent"),
        (None, None) => return "~/.omp/agent".to_owned(),
    };
    match home.and_then(|h| dir.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    }
}

/// Number field text → JSON number (integers stay integers); rejects NaN/inf and junk.
pub fn parse_number(text: &str) -> Result<Value, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("Enter a number".to_owned());
    }
    if let Ok(i) = t.parse::<i64>() {
        return Ok(Value::from(i));
    }
    match t.parse::<f64>() {
        Ok(f) if f.is_finite() => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .ok_or_else(|| "Not a valid number".to_owned()),
        Ok(_) => Err("Number must be finite".to_owned()),
        Err(_) => Err("Not a valid number".to_owned()),
    }
}

/// JSON editor text → value, which must be an array (`Array`) or an object (`Record`).
pub fn parse_json_for(kind: &SettingKind, text: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("Invalid JSON: {e}"))?;
    match (kind, &value) {
        (SettingKind::Array, Value::Array(_)) | (SettingKind::Record, Value::Object(_)) => {
            Ok(value)
        }
        (SettingKind::Array, _) => Err("Expected a JSON array ([ … ])".to_owned()),
        (SettingKind::Record, _) => Err("Expected a JSON object ({ … })".to_owned()),
        _ => Err("This setting does not take JSON".to_owned()),
    }
}

/// Text shown in a number/string field for the stored value.
pub fn value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

/// One-line compact JSON, cut to `max` characters.
pub fn preview(value: Option<&Value>, max: usize) -> String {
    let text = match value {
        None | Some(Value::Null) => return "empty".to_owned(),
        Some(Value::Array(a)) if a.is_empty() => return "[ ]".to_owned(),
        Some(Value::Object(o)) if o.is_empty() => return "{ }".to_owned(),
        Some(v) => v.to_string(),
    };
    if text.chars().count() <= max {
        text
    } else {
        let cut: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Pretty JSON for the modal editor (empty array/object when unset).
pub fn pretty_json(kind: &SettingKind, value: Option<&Value>) -> String {
    let fallback = match kind {
        SettingKind::Record => Value::Object(serde_json::Map::new()),
        _ => Value::Array(Vec::new()),
    };
    serde_json::to_string_pretty(value.filter(|v| !v.is_null()).unwrap_or(&fallback))
        .unwrap_or_else(|_| "[]".to_owned())
}

const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The default model (`modelRoles.default`, an optional `:level` suffix split off) and thinking
/// level (`defaultThinkingLevel`) a fresh omp session starts with.
pub fn defaults_from_catalog(catalog: &SettingsCatalog) -> Defaults {
    let value = |key: &str| {
        catalog
            .tabs
            .iter()
            .flat_map(|t| &t.settings)
            .find(|s| s.key == key)
            .and_then(|s| s.value.clone().or_else(|| s.default.clone()))
    };
    let role = value("modelRoles")
        .and_then(|v| v.get("default").and_then(Value::as_str).map(str::to_owned))
        .filter(|m| !m.trim().is_empty());
    let (model, suffix) = match role.as_deref().and_then(|r| r.rsplit_once(':')) {
        Some((m, l)) if l == "auto" || THINKING_LEVELS.contains(&l) => {
            (Some(m.to_owned()), Some(l.to_owned()))
        }
        _ => (role, None),
    };
    let level = suffix.or_else(|| {
        value("defaultThinkingLevel")
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|l| !l.is_empty())
    });
    Defaults { model, level }
}

#[cfg(test)]
mod tests {
    use deck_core::omp::SettingsTab;
    use serde_json::json;

    use super::*;

    fn setting(
        key: &str,
        kind: SettingKind,
        value: Option<Value>,
        default: Option<Value>,
    ) -> Setting {
        Setting {
            key: key.into(),
            kind,
            value,
            default,
            description: format!("About {key}"),
            secret: false,
        }
    }

    fn model() -> Model {
        let catalog = SettingsCatalog {
            tabs: vec![
                SettingsTab {
                    id: "appearance".into(),
                    label: "Appearance".into(),
                    settings: vec![
                        setting(
                            "theme.dark",
                            SettingKind::String,
                            None,
                            Some(json!("titanium")),
                        ),
                        setting("statusLine.preset", SettingKind::String, None, None),
                        setting(
                            "showHints",
                            SettingKind::Bool,
                            Some(json!(true)),
                            Some(json!(false)),
                        ),
                        setting("theme.light", SettingKind::String, None, None),
                    ],
                },
                SettingsTab {
                    id: "shell".into(),
                    label: "Shell".into(),
                    settings: vec![setting("shell.path", SettingKind::String, None, None)],
                },
            ],
        };
        Model::from_catalog(catalog, "~/.omp/agent".into())
    }

    fn catalog_with(settings: Vec<Setting>) -> SettingsCatalog {
        SettingsCatalog {
            tabs: vec![SettingsTab {
                id: "model".into(),
                label: "Model".into(),
                settings,
            }],
        }
    }

    #[test]
    fn defaults_come_from_model_role_and_thinking_setting() {
        let c = catalog_with(vec![
            setting(
                "modelRoles",
                SettingKind::Record,
                Some(json!({"default": "fake/fake-1", "smol": "x/y"})),
                None,
            ),
            setting(
                "defaultThinkingLevel",
                SettingKind::String,
                Some(json!("high")),
                Some(json!("high")),
            ),
        ]);
        assert_eq!(
            defaults_from_catalog(&c),
            Defaults {
                model: Some("fake/fake-1".into()),
                level: Some("high".into())
            }
        );
        // A `:level` suffix on the role wins and is not part of the selector.
        let c = catalog_with(vec![setting(
            "modelRoles",
            SettingKind::Record,
            Some(json!({"default": "a/b:low"})),
            None,
        )]);
        assert_eq!(
            defaults_from_catalog(&c),
            Defaults {
                model: Some("a/b".into()),
                level: Some("low".into())
            }
        );
        // Nothing configured.
        assert_eq!(
            defaults_from_catalog(&catalog_with(Vec::new())),
            Defaults::default()
        );
    }

    #[test]
    fn search_is_case_insensitive_multi_word_and_grouped_by_tab() {
        let m = model();
        let hits = m.search("THEME dark");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].tab, 0);
        assert_eq!(hits[0].rows, [0]);

        // Matches key, label and description; terms may hit different fields.
        assert_eq!(m.search("preset line").len(), 1);
        assert_eq!(m.search("about shell.path")[0].tab, 1);
        let all = m.search("about");
        assert_eq!(all.len(), 2);
        assert!(m.search("nothing-like-this").is_empty());
        assert!(m.search("   ").is_empty());
    }

    #[test]
    fn rows_group_by_section_in_first_appearance_order() {
        let m = model();
        let titles: Vec<&str> = m.tabs[0]
            .sections
            .iter()
            .map(|s| s.title.as_str())
            .collect();
        assert_eq!(titles, ["Theme", "Status line", "General"]);
        assert_eq!(m.tabs[0].sections[0].rows, [0, 3]);
    }

    #[test]
    fn section_titles() {
        assert_eq!(section_title(""), "General");
        assert_eq!(section_title("statusLine"), "Status line");
        assert_eq!(section_title("a.bC"), "A › B c");
    }

    #[test]
    fn number_parsing() {
        assert_eq!(parse_number(" 42 "), Ok(json!(42)));
        assert_eq!(parse_number("-3"), Ok(json!(-3)));
        assert_eq!(parse_number("0.5"), Ok(json!(0.5)));
        assert_eq!(parse_number("1e3"), Ok(json!(1000.0)));
        assert!(parse_number("").is_err());
        assert!(parse_number("abc").is_err());
        assert!(parse_number("NaN").is_err());
        assert!(parse_number("inf").is_err());
    }

    #[test]
    fn json_editor_validation() {
        assert_eq!(
            parse_json_for(&SettingKind::Array, "[1, \"a\"]"),
            Ok(json!([1, "a"]))
        );
        assert!(parse_json_for(&SettingKind::Array, "{}").is_err());
        assert!(parse_json_for(&SettingKind::Record, "[]").is_err());
        assert_eq!(
            parse_json_for(&SettingKind::Record, "{\"a\": 1}"),
            Ok(json!({"a": 1}))
        );
        assert!(parse_json_for(&SettingKind::Record, "{oops").is_err());
        assert!(parse_json_for(&SettingKind::Record, "").is_err());
    }

    #[test]
    fn write_results_update_the_row() {
        let mut m = model();
        let row = m.find_mut("theme.dark").expect("row");
        row.begin(Some(json!("graphite")));
        assert!(row.pending);
        row.finish(Ok(Some(Value::Null)));
        assert_eq!(
            row.setting.value,
            Some(json!("graphite")),
            "null echo keeps the sent value"
        );
        assert!(row.setting.is_modified());

        row.begin(Some(json!("x")));
        row.finish(Err("bad value".into()));
        assert_eq!(
            row.setting.value,
            Some(json!("graphite")),
            "failure keeps the old value"
        );
        assert_eq!(row.error.as_deref(), Some("bad value"));
        assert!(!row.pending);

        row.begin(None);
        row.finish(Ok(None));
        assert_eq!(row.setting.value, None);
        assert!(!row.setting.is_modified());
        assert!(row.error.is_none());
    }

    #[test]
    fn secrets_are_never_stored() {
        let mut s = setting("auth.token", SettingKind::String, None, None);
        s.secret = true;
        let mut row = Row::new(s);
        row.begin(Some(json!("hunter2")));
        assert!(
            row.sent.is_none(),
            "a secret is not retained for the fallback"
        );
        row.finish(Ok(Some(Value::Null)));
        assert_eq!(row.setting.value, Some(json!(SECRET_PLACEHOLDER)));
        assert!(row.sent.is_none(), "a secret never outlives the write call");
    }

    #[test]
    fn config_dir_banner_path() {
        let home = Path::new("/home/u");
        assert_eq!(config_dir_display(None, Some(home)), "~/.omp/agent");
        assert_eq!(
            config_dir_display(Some("/home/u/x/agent"), Some(home)),
            "~/x/agent"
        );
        assert_eq!(
            config_dir_display(Some("/tmp/agent"), Some(home)),
            "/tmp/agent"
        );
        assert_eq!(config_dir_display(Some(""), Some(home)), "~/.omp/agent");
        assert_eq!(config_dir_display(None, None), "~/.omp/agent");
    }

    #[test]
    fn previews() {
        assert_eq!(preview(None, 20), "empty");
        assert_eq!(preview(Some(&json!([])), 20), "[ ]");
        assert_eq!(preview(Some(&json!({"a": 1})), 20), "{\"a\":1}");
        assert_eq!(preview(Some(&json!(["abcdefghij"])), 6), "[\"abc…");
        assert_eq!(value_text(Some(&json!(7))), "7");
        assert_eq!(value_text(Some(&json!("s"))), "s");
        assert_eq!(pretty_json(&SettingKind::Record, None), "{}");
        assert_eq!(pretty_json(&SettingKind::Array, None), "[]");
    }
}
