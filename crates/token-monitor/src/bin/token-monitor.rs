use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use token_monitor_core::{
    credentials, sort_burn_first, usage, Availability, LimitWindow, ProviderSnapshot, SourceHealth,
    WindowKind, WindowMetric,
};

const GREY: Color = Color::Rgb(145, 145, 155);
const GREEN: Color = Color::Rgb(0, 220, 150);
const RED: Color = Color::Rgb(255, 90, 100);
const YELLOW: Color = Color::Rgb(245, 205, 45);
const CYAN: Color = Color::Rgb(125, 207, 255);
const BLUE: Color = Color::Rgb(122, 162, 247);
const _PURPLE: Color = Color::Rgb(187, 154, 247);
const DIM_GREY: Color = Color::Rgb(86, 95, 137);

#[derive(Debug, Parser)]
#[command(
    name = "token-monitor",
    version,
    about = "Native terminal token monitor"
)]
struct Args {
    /// Render one snapshot and exit.
    #[arg(long)]
    once: bool,
    /// Show the local usage/ledger report instead of the limits view.
    #[arg(long, alias = "t")]
    consumption: bool,
    /// Collect live balances for the currently implemented provider slice (enabled by default).
    #[arg(long, overrides_with = "mock")]
    live: bool,
    /// Disable live balance collection and use static mock fixtures.
    #[arg(long, alias = "no-live", overrides_with = "live")]
    mock: bool,
    /// Refresh live collectors at this many seconds (minimum 10).
    #[arg(long, default_value_t = 60)]
    refresh: u64,
    /// Restrict live collection to a comma-separated provider list.
    #[arg(long, value_delimiter = ',')]
    providers: Option<Vec<String>>,
    /// Emit normalized limits JSON (or a usage report with --consumption).
    #[arg(long)]
    json: bool,
    /// Include per-record pricing evidence in consumption JSON (large output).
    #[arg(long)]
    audit_pricing: bool,
    /// Restrict local usage parsing to a comma-separated client list.
    #[arg(long, value_delimiter = ',')]
    clients: Option<Vec<String>>,
    /// Number of recent calendar days to include (0 means all local history).
    #[arg(long, default_value_t = 30)]
    days: u32,
    /// Force a terminal width for layout smoke tests.
    #[arg(long, short = 'w')]
    width: Option<u16>,
    /// Disable ANSI terminal colors.
    #[arg(long)]
    no_color: bool,
    /// Show account identifiers in terminal/JSON output (masked by default).
    #[arg(long)]
    show_account: bool,
    /// Skip the one-time read-only import of legacy GUI/Node history.
    #[arg(long)]
    skip_import: bool,
}

impl Args {
    fn resolve_live(&mut self) {
        if !self.mock {
            self.live = true;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Limits,
    Consumption,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConsumptionMetric {
    Tokens,
    ApiEquivalent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    All,
    Attention,
    Credits,
    Quotas,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialKind {
    ApiKey,
    OAuth,
    Cookie,
}

impl CredentialKind {
    fn label(self) -> &'static str {
        match self {
            Self::ApiKey => "API key",
            Self::OAuth => "OAuth token",
            Self::Cookie => "session cookie/header",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CredentialSpec {
    provider_id: &'static str,
    env_name: &'static str,
    kind: CredentialKind,
    url: &'static str,
    prefix: &'static str,
    instruction: &'static str,
}

const CREDENTIAL_SPECS: &[CredentialSpec] = &[
    CredentialSpec {
        provider_id: "claude",
        env_name: "TOKEN_MONITOR_CLAUDE_WEB_COOKIE",
        kind: CredentialKind::Cookie,
        url: "claude.ai",
        prefix: "",
        instruction:
            "Copy the authenticated claude.ai Cookie header (usually contains sessionKey=...). A Claude Code OAuth token is also accepted here.",
    },
    CredentialSpec {
        provider_id: "commandcode",
        env_name: "TOKEN_MONITOR_COMMANDCODE_COOKIE",
        kind: CredentialKind::Cookie,
        url: "commandcode.ai",
        prefix: "",
        instruction: "Copy the authenticated Cookie header from the Command Code session.",
    },
    CredentialSpec {
        provider_id: "openrouter",
        env_name: "TOKEN_MONITOR_OPENROUTER_API_KEY",
        kind: CredentialKind::ApiKey,
        url: "openrouter.ai/settings/keys",
        prefix: "sk-or-",
        instruction: "Create a key with the smallest scope that can read your balance.",
    },
    CredentialSpec {
        provider_id: "deepseek",
        env_name: "TOKEN_MONITOR_DEEPSEEK_API_KEY",
        kind: CredentialKind::ApiKey,
        url: "platform.deepseek.com/api_keys",
        prefix: "",
        instruction: "Paste the DeepSeek API key. The balance endpoint is read-only.",
    },
    CredentialSpec {
        provider_id: "zai",
        env_name: "TOKEN_MONITOR_ZAI_API_KEY",
        kind: CredentialKind::ApiKey,
        url: "z.ai",
        prefix: "",
        instruction: "Paste the z.ai API key for quota lookup.",
    },
    CredentialSpec {
        provider_id: "zaiteam",
        env_name: "TOKEN_MONITOR_ZAI_TEAM_API_KEY",
        kind: CredentialKind::ApiKey,
        url: "z.ai",
        prefix: "",
        instruction: "Paste the z.ai team API key; organization/project remain local settings.",
    },
    CredentialSpec {
        provider_id: "minimax",
        env_name: "TOKEN_MONITOR_MINIMAX_API_KEY",
        kind: CredentialKind::ApiKey,
        url: "platform.minimaxi.com",
        prefix: "",
        instruction: "Paste the MiniMax coding-plan API key.",
    },
    CredentialSpec {
        provider_id: "copilot",
        env_name: "TOKEN_MONITOR_COPILOT_API_TOKEN",
        kind: CredentialKind::ApiKey,
        url: "github.com/settings/copilot",
        prefix: "ghp_ / github_pat_",
        instruction: "Paste a GitHub token permitted to read Copilot usage.",
    },
    CredentialSpec {
        provider_id: "qoder",
        env_name: "TOKEN_MONITOR_QODER_COOKIE",
        kind: CredentialKind::Cookie,
        url: "qoder.com",
        prefix: "",
        instruction: "Paste the authenticated Qoder Cookie header.",
    },
    CredentialSpec {
        provider_id: "trae",
        env_name: "TOKEN_MONITOR_TRAE_ACCESS_TOKEN",
        kind: CredentialKind::OAuth,
        url: "trae.ai",
        prefix: "",
        instruction: "Paste the Trae Cloud-IDE-JWT access token.",
    },
    CredentialSpec {
        provider_id: "ollama",
        env_name: "TOKEN_MONITOR_OLLAMA_COOKIE",
        kind: CredentialKind::Cookie,
        url: "ollama.com/settings",
        prefix: "",
        instruction: "Paste the Ollama Cloud session cookie.",
    },
    CredentialSpec {
        provider_id: "kimi",
        env_name: "TOKEN_MONITOR_KIMI_API_KEY",
        kind: CredentialKind::ApiKey,
        url: "kimi.com",
        prefix: "",
        instruction: "Paste the Kimi Code API key. Adapter parity is still pending.",
    },
    CredentialSpec {
        provider_id: "mimo",
        env_name: "TOKEN_MONITOR_MIMO_COOKIE",
        kind: CredentialKind::Cookie,
        url: "platform.xiaomimimo.com",
        prefix: "",
        instruction: "Paste the MiMo session cookie. Adapter parity is still pending.",
    },
    CredentialSpec {
        provider_id: "opencode",
        env_name: "TOKEN_MONITOR_OPENCODE_API_KEY",
        kind: CredentialKind::ApiKey,
        url: "opencode.ai",
        prefix: "",
        instruction: "Paste the OpenCode credential. Adapter parity is still pending.",
    },
    CredentialSpec {
        provider_id: "workbuddy",
        env_name: "TOKEN_MONITOR_WORKBUDDY_COOKIE",
        kind: CredentialKind::Cookie,
        url: "workbuddy.ai",
        prefix: "",
        instruction: "Paste the WorkBuddy session cookie. Adapter parity is still pending.",
    },
];

fn credential_spec(provider_id: &str) -> Option<&'static CredentialSpec> {
    CREDENTIAL_SPECS
        .iter()
        .find(|spec| spec.provider_id == provider_id)
}


#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LedgerWindow {
    AllTime,
    Today,
    Last24h,
    Last7d,
    Last30d,
}

impl LedgerWindow {
    pub fn label(&self) -> &'static str {
        match self {
            LedgerWindow::Today => "Today",
            LedgerWindow::Last24h => "Last 24h",
            LedgerWindow::Last7d => "Last 7d",
            LedgerWindow::Last30d => "Last 30d",
            LedgerWindow::AllTime => "All Time",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            LedgerWindow::AllTime => LedgerWindow::Today,
            LedgerWindow::Today => LedgerWindow::Last24h,
            LedgerWindow::Last24h => LedgerWindow::Last7d,
            LedgerWindow::Last7d => LedgerWindow::Last30d,
            LedgerWindow::Last30d => LedgerWindow::AllTime,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowClientItem {
    pub client: String,
    pub tokens: i64,
    pub api_usd: f64,
    pub calls: usize,
    pub models: Vec<(String, i64, f64, usize)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DetailModal {
    None,
    LimitsProvider(Box<ProviderSnapshot>),
    ConsumptionWindow {
        window: LedgerWindow,
        records: usize,
        tokens: usage::UsageTokens,
        api_usd: f64,
        clients: Vec<WindowClientItem>,
        models: Vec<(String, String, i64, f64, usize)>,
    },
    ConsumptionClient {
        client: String,
        window: LedgerWindow,
        records: usize,
        tokens: usage::UsageTokens,
        api_usd: f64,
        models: Vec<(String, i64, f64, usize)>,
    },
    ConsumptionModel {
        model: String,
        window: LedgerWindow,
        records: usize,
        tokens: usage::UsageTokens,
        api_usd: f64,
        clients: Vec<(String, i64, f64, usize)>,
    },
}

struct SettingsState {
    provider_ids: Vec<String>,
    selected: usize,
    editing: bool,
    input: String,
    message: String,
}

impl SettingsState {
    fn new() -> Self {
        Self {
            provider_ids: token_monitor_core::provider_registry::ALL_PROVIDER_IDS
                .iter()
                .map(|id| (*id).to_owned())
                .collect(),
            selected: 0,
            editing: false,
            input: String::new(),
            message: String::new(),
        }
    }
}

struct CachedLedger {
    buckets: Vec<TemporalUsageBucket>,
    clients: [Vec<ConsumptionRow>; 5],
    models: [Vec<ModelConsumptionRow>; 5],
}

impl CachedLedger {
    fn from_report(report: &usage::ConsumptionReport) -> Self {
        let buckets = compute_temporal_usage(report);
        let windows = [
            LedgerWindow::Today,
            LedgerWindow::Last24h,
            LedgerWindow::Last7d,
            LedgerWindow::Last30d,
            LedgerWindow::AllTime,
        ];
        let mut clients: [Vec<ConsumptionRow>; 5] = Default::default();
        let mut models: [Vec<ModelConsumptionRow>; 5] = Default::default();

        for (idx, w) in windows.iter().enumerate() {
            clients[idx] = consumption_rows(report, ConsumptionMetric::Tokens, *w, "");
            models[idx] = model_consumption_rows(report, ConsumptionMetric::Tokens, *w, "");
        }

        Self {
            buckets,
            clients,
            models,
        }
    }
}

struct App {
    providers: Vec<ProviderSnapshot>,
    consumption: Option<usage::ConsumptionReport>,
    cached_ledger: Option<CachedLedger>,
    usage_busy: bool,
    limits_busy: bool,
    view: View,
    filter: Filter,
    scroll: u16,
    refreshes: u32,
    last_refresh: Instant,
    should_quit: bool,
    no_color: bool,
    limits_selected: usize,
    consumption_selected: usize,
    modal_scroll: u16,
    modal: DetailModal,
    ledger_window: LedgerWindow,
    search_query: String,
    searching: bool,
    live: bool,
    show_account: bool,
    consumption_metric: ConsumptionMetric,
    settings: SettingsState,
    refresh_requested: bool,
}

impl App {
    fn set_consumption(&mut self, report: usage::ConsumptionReport) {
        self.cached_ledger = Some(CachedLedger::from_report(&report));
        self.consumption = Some(report);
    }

    fn open_limits_detail(&mut self) {
        let visible = self.visible_providers();
        if visible.is_empty() {
            return;
        }
        let mut subscriptions = Vec::new();
        let mut wallets = Vec::new();
        for p in visible {
            if is_wallet_provider(p) {
                wallets.push(p);
            } else {
                subscriptions.push(p);
            }
        }
        let ordered: Vec<&ProviderSnapshot> = subscriptions.into_iter().chain(wallets).collect();
        let sel = self.limits_selected.min(ordered.len().saturating_sub(1));
        let selected_provider = ordered.get(sel).cloned().cloned();
        if let Some(p) = selected_provider {
            self.modal_scroll = 0;
            self.modal = DetailModal::LimitsProvider(Box::new(p));
        }
    }

    fn open_consumption_detail(&mut self) {
        let Some(report) = &self.consumption else { return; };
        let cur_window = self.ledger_window;
        let rows = consumption_rows(report, self.consumption_metric, cur_window, &self.search_query);
        let m_rows = model_consumption_rows(report, self.consumption_metric, cur_window, &self.search_query);
        let num_windows = 1;
        let total_selectable = num_windows + rows.len() + m_rows.len();
        if total_selectable == 0 { return; }
        let sel = self.consumption_selected.min(total_selectable - 1);

        if sel == 0 {
            let target_win = cur_window;

            let now_ms = chrono::Utc::now().timestamp_millis();
            let today_str = local_today_str();
            let mut win_tokens = usage::UsageTokens::default();
            let mut win_records = 0;
            let mut win_usd = 0.0;
            let mut client_map: HashMap<String, (i64, f64, usize, HashMap<String, (i64, f64, usize)>)> = HashMap::new();
            let mut model_map: HashMap<String, (i64, f64, usize, HashMap<String, usize>)> = HashMap::new();
            let pricing = token_monitor_core::pricing::PricingEngine::load_cached();

            for (idx, rec) in report.snapshot.records.iter().enumerate() {
                if !record_in_window(rec, target_win, now_ms, &today_str) {
                    continue;
                }
                win_records += 1;
                win_tokens.add_assign(&rec.tokens);
                let val = report.quotes.as_ref().and_then(|q| q.get(idx)).and_then(|q| q.value_usd).unwrap_or_else(|| pricing.quote(rec).value_usd.unwrap_or(0.0));
                win_usd += val;
                let c_norm = normalize_client_id(&rec.client);
                let c_entry = client_map.entry(c_norm.clone()).or_insert_with(|| (0, 0.0, 0, HashMap::new()));
                c_entry.0 += rec.tokens.reported_total_without_reasoning();
                c_entry.1 += val;
                c_entry.2 += 1;
                let sub_m = c_entry.3.entry(rec.model_id.clone()).or_insert((0, 0.0, 0));
                sub_m.0 += rec.tokens.reported_total_without_reasoning();
                sub_m.1 += val;
                sub_m.2 += 1;

                let m_entry = model_map.entry(rec.model_id.clone()).or_insert_with(|| (0, 0.0, 0, HashMap::new()));
                m_entry.0 += rec.tokens.reported_total_without_reasoning();
                m_entry.1 += val;
                m_entry.2 += 1;
                *m_entry.3.entry(c_norm).or_insert(0) += 1;
            }

            let mut c_vec: Vec<WindowClientItem> = client_map.into_iter().map(|(c, (t, u, n, sub_map))| {
                let mut sub_vec: Vec<(String, i64, f64, usize)> = sub_map.into_iter().map(|(m, (st, su, sn))| (m, st, su, sn)).collect();
                sub_vec.sort_by_key(|a| std::cmp::Reverse(a.1));
                WindowClientItem {
                    client: c,
                    tokens: t,
                    api_usd: u,
                    calls: n,
                    models: sub_vec,
                }
            }).collect();
            c_vec.sort_by_key(|a| std::cmp::Reverse(a.tokens));

            let mut m_vec: Vec<(String, String, i64, f64, usize)> = model_map.into_iter().map(|(m, (t, u, n, c_map))| {
                let mut c_list: Vec<(String, usize)> = c_map.into_iter().collect();
                c_list.sort_by_key(|a| std::cmp::Reverse(a.1));
                let c_str = c_list.into_iter().map(|(c, _)| c).collect::<Vec<_>>().join(", ");
                (m, c_str, t, u, n)
            }).collect();
            m_vec.sort_by_key(|a| std::cmp::Reverse(a.2));

            self.modal_scroll = 0;
            self.modal = DetailModal::ConsumptionWindow {
                window: target_win,
                records: win_records,
                tokens: win_tokens,
                api_usd: win_usd,
                clients: c_vec,
                models: m_vec,
            };
        } else if sel < num_windows + rows.len() {
            let row_idx = sel - num_windows;
            let row = &rows[row_idx];
            let target_client = row.client.clone();
            let mut client_models: std::collections::HashMap<String, (i64, f64, usize)> = std::collections::HashMap::new();
            let pricing = token_monitor_core::pricing::PricingEngine::load_cached();
            let now_ms = chrono::Utc::now().timestamp_millis();
            let today_str = local_today_str();

            for (idx, rec) in report.snapshot.records.iter().enumerate() {
                if !record_in_window(rec, cur_window, now_ms, &today_str) {
                    continue;
                }
                if normalize_client_id(&rec.client) == target_client {
                    let entry = client_models.entry(rec.model_id.clone()).or_insert((0, 0.0, 0));
                    entry.0 += rec.tokens.reported_total_without_reasoning();
                    let val = report.quotes.as_ref().and_then(|q| q.get(idx)).and_then(|q| q.value_usd).unwrap_or_else(|| pricing.quote(rec).value_usd.unwrap_or(0.0));
                    entry.1 += val;
                    entry.2 += 1;
                }
            }
            let mut models_vec: Vec<(String, i64, f64, usize)> = client_models.into_iter().map(|(m, (t, u, c))| (m, t, u, c)).collect();
            models_vec.sort_by_key(|a| std::cmp::Reverse(a.1));

            self.modal_scroll = 0;
            self.modal = DetailModal::ConsumptionClient {
                client: target_client,
                window: cur_window,
                records: row.records,
                tokens: row.tokens.clone(),
                api_usd: row.api_value_usd,
                models: models_vec,
            };
        } else {
            let m_idx = sel - num_windows - rows.len();
            let m_row = &m_rows[m_idx];
            let target_model = m_row.model.clone();
            let mut model_clients: std::collections::HashMap<String, (i64, f64, usize)> = std::collections::HashMap::new();
            let pricing = token_monitor_core::pricing::PricingEngine::load_cached();
            let now_ms = chrono::Utc::now().timestamp_millis();
            let today_str = local_today_str();

            for (idx, rec) in report.snapshot.records.iter().enumerate() {
                if !record_in_window(rec, cur_window, now_ms, &today_str) {
                    continue;
                }
                if rec.model_id == target_model {
                    let c_norm = normalize_client_id(&rec.client);
                    let entry = model_clients.entry(c_norm).or_insert((0, 0.0, 0));
                    entry.0 += rec.tokens.reported_total_without_reasoning();
                    let val = report.quotes.as_ref().and_then(|q| q.get(idx)).and_then(|q| q.value_usd).unwrap_or_else(|| pricing.quote(rec).value_usd.unwrap_or(0.0));
                    entry.1 += val;
                    entry.2 += 1;
                }
            }
            let mut clients_vec: Vec<(String, i64, f64, usize)> = model_clients.into_iter().map(|(c, (t, u, k))| (c, t, u, k)).collect();
            clients_vec.sort_by_key(|a| std::cmp::Reverse(a.1));

            self.modal_scroll = 0;
            self.modal = DetailModal::ConsumptionModel {
                model: target_model,
                window: cur_window,
                records: m_row.records,
                tokens: m_row.tokens.clone(),
                api_usd: m_row.api_value_usd,
                clients: clients_vec,
            };
        }
    }

    fn new(no_color: bool, show_account: bool) -> Self {
        let mut providers = fixtures();
        sort_burn_first(&mut providers, chrono::Utc::now().timestamp_millis());
        let mut app = Self {
            providers,
            consumption: None,
            cached_ledger: None,
            usage_busy: false,
            limits_busy: false,
            view: View::Limits,
            filter: Filter::All,
            scroll: 0,
            refreshes: 0,
            last_refresh: Instant::now(),
            should_quit: false,
            no_color,
            live: false,
            show_account,
            consumption_metric: ConsumptionMetric::Tokens,
            settings: SettingsState::new(),
            refresh_requested: false,
            limits_selected: 0,
            consumption_selected: 0,
            modal_scroll: 0,
            modal: DetailModal::None,
            ledger_window: LedgerWindow::Today,
            search_query: String::new(),
            searching: false,
        };
        let report = mock_consumption_report();
        app.set_consumption(report);
        app
    }

    fn from_providers(
        mut providers: Vec<ProviderSnapshot>,
        no_color: bool,
        show_account: bool,
    ) -> Self {
        sort_burn_first(&mut providers, chrono::Utc::now().timestamp_millis());
        Self {
            providers,
            consumption: None,
            cached_ledger: None,
            usage_busy: false,
            limits_busy: false,
            view: View::Limits,
            filter: Filter::All,
            scroll: 0,
            refreshes: 0,
            last_refresh: Instant::now(),
            should_quit: false,
            no_color,
            live: true,
            show_account,
            consumption_metric: ConsumptionMetric::Tokens,
            settings: SettingsState::new(),
            refresh_requested: false,
            limits_selected: 0,
            consumption_selected: 0,
            modal_scroll: 0,
            modal: DetailModal::None,
            ledger_window: LedgerWindow::Today,
            search_query: String::new(),
            searching: false,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        if self.modal != DetailModal::None {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char(' ') => {
                    self.modal = DetailModal::None;
                    self.modal_scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.modal_scroll = self.modal_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.modal_scroll = self.modal_scroll.saturating_add(1);
                }
                _ => {}
            }
            return;
        }

        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.search_query.clear();
                    self.limits_selected = 0;
                    self.consumption_selected = 0;
                    self.scroll = 0;
                }
                KeyCode::Enter => {
                    self.searching = false;
                    self.limits_selected = 0;
                    self.consumption_selected = 0;
                    self.scroll = 0;
                }
                KeyCode::Backspace => {
                    self.search_query.pop();
                    self.limits_selected = 0;
                    self.consumption_selected = 0;
                    self.scroll = 0;
                }
                KeyCode::Char(c) => {
                    self.search_query.push(c);
                    self.limits_selected = 0;
                    self.consumption_selected = 0;
                    self.scroll = 0;
                }
                _ => {}
            }
            return;
        }
        if self.view == View::Settings {
            self.handle_settings_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('/') => {
                self.searching = true;
            }
            KeyCode::Esc => {
                if !self.search_query.is_empty() {
                    self.search_query.clear();
                    self.limits_selected = 0;
                    self.consumption_selected = 0;
                    self.scroll = 0;
                } else {
                    self.view = View::Limits;
                    self.scroll = 0;
                }
            }
            KeyCode::Char('1') => {
                self.view = View::Limits;
                self.scroll = 0;
            }
            KeyCode::Char('2') => {
                self.view = View::Consumption;
                self.scroll = 0;
            }
            KeyCode::Char('3') => self.open_settings(),
            KeyCode::Tab => {
                self.view = match self.view {
                    View::Limits => View::Consumption,
                    View::Consumption => View::Settings,
                    View::Settings => View::Limits,
                };
                self.scroll = 0;
            }
            KeyCode::BackTab => {
                self.view = match self.view {
                    View::Limits => View::Settings,
                    View::Consumption => View::Limits,
                    View::Settings => View::Consumption,
                };
                self.scroll = 0;
            }
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('t') => {
                self.view = match self.view {
                    View::Limits => View::Consumption,
                    View::Consumption => View::Limits,
                    View::Settings => View::Limits,
                };
                self.scroll = 0;
            }
            KeyCode::Char('w') if self.view == View::Consumption => {
                self.ledger_window = self.ledger_window.next();
                self.scroll = 0;
            }
            KeyCode::Char('m') if self.view == View::Consumption => {
                self.consumption_metric = match self.consumption_metric {
                    ConsumptionMetric::Tokens => ConsumptionMetric::ApiEquivalent,
                    ConsumptionMetric::ApiEquivalent => ConsumptionMetric::Tokens,
                };
                self.scroll = 0;
            }
            KeyCode::Char('p') | KeyCode::Char('s') => self.open_settings(),
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.refreshes = self.refreshes.saturating_add(1);
                self.last_refresh = Instant::now();
                if self.view == View::Consumption {
                    self.usage_busy = true;
                } else {
                    self.limits_busy = true;
                    self.refresh_requested = true;
                }
            }
            KeyCode::Char('a') => {
                self.filter = if self.filter == Filter::Attention {
                    Filter::All
                } else {
                    Filter::Attention
                }
            }
            KeyCode::Char('c') => {
                self.filter = if self.filter == Filter::Credits {
                    Filter::All
                } else {
                    Filter::Credits
                }
            }
            KeyCode::Char('u') => {
                self.filter = if self.filter == Filter::Quotas {
                    Filter::All
                } else {
                    Filter::Quotas
                }
            }
            KeyCode::Enter => {
                if self.view == View::Limits {
                    self.open_limits_detail();
                } else if self.view == View::Consumption {
                    self.open_consumption_detail();
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if self.view == View::Consumption {
                    self.ledger_window = match self.ledger_window {
                        LedgerWindow::Today => LedgerWindow::Today,
                        LedgerWindow::Last24h => LedgerWindow::Today,
                        LedgerWindow::Last7d => LedgerWindow::Last24h,
                        LedgerWindow::Last30d => LedgerWindow::Last7d,
                        LedgerWindow::AllTime => LedgerWindow::Last30d,
                    };
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if self.view == View::Consumption {
                    self.ledger_window = match self.ledger_window {
                        LedgerWindow::Today => LedgerWindow::Last24h,
                        LedgerWindow::Last24h => LedgerWindow::Last7d,
                        LedgerWindow::Last7d => LedgerWindow::Last30d,
                        LedgerWindow::Last30d => LedgerWindow::AllTime,
                        LedgerWindow::AllTime => LedgerWindow::AllTime,
                    };
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.view == View::Limits {
                    self.limits_selected = self.limits_selected.saturating_sub(1);
                } else if self.view == View::Consumption {
                    self.consumption_selected = self.consumption_selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.view == View::Limits {
                    let total = self.visible_providers().len();
                    if total > 0 {
                        self.limits_selected = (self.limits_selected + 1).min(total - 1);
                    }
                } else if self.view == View::Consumption {
                    let (num_clients, num_models) = if let Some(c) = &self.cached_ledger {
                        let win_idx = match self.ledger_window {
                            LedgerWindow::Today => 0,
                            LedgerWindow::Last24h => 1,
                            LedgerWindow::Last7d => 2,
                            LedgerWindow::Last30d => 3,
                            LedgerWindow::AllTime => 4,
                        };
                        (c.clients[win_idx].len(), c.models[win_idx].len())
                    } else if let Some(r) = &self.consumption {
                        (
                            consumption_rows(r, self.consumption_metric, self.ledger_window, &self.search_query).len(),
                            model_consumption_rows(r, self.consumption_metric, self.ledger_window, &self.search_query).len(),
                        )
                    } else {
                        (0, 0)
                    };
                    let total = 1 + num_clients + num_models;
                    if total > 0 {
                        self.consumption_selected = (self.consumption_selected + 1).min(total - 1);
                    }
                }
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(8),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(8),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::Char('G') => self.scroll = u16::MAX,
            _ => {}
        }
    }

    fn open_settings(&mut self) {
        self.view = View::Settings;
        self.settings.editing = false;
        self.settings.input.clear();
        self.settings.message.clear();
        self.settings.selected = self
            .settings
            .provider_ids
            .iter()
            .position(|provider_id| {
                self.providers.iter().any(|provider| {
                    provider.provider_id == *provider_id
                        && (provider.availability == Availability::Unknown
                            || provider.source_health != SourceHealth::Connected)
                })
            })
            .unwrap_or(0);
        self.scroll = 0;
    }

    fn selected_credential_spec(&self) -> Option<&'static CredentialSpec> {
        self.settings
            .provider_ids
            .get(self.settings.selected)
            .and_then(|provider_id| credential_spec(provider_id))
    }

    fn settings_status(&self, provider_id: &str) -> String {
        if let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.provider_id == provider_id)
        {
            if provider.availability != Availability::Unknown {
                return provider.availability.label().to_owned();
            }
            return provider.source_health.label().to_owned();
        }
        if credential_spec(provider_id).is_some_and(|spec| credentials::has(spec.env_name)) {
            "saved · refresh".into()
        } else if credential_spec(provider_id).is_some() {
            "not configured".into()
        } else {
            "auto-discover".into()
        }
    }

    fn begin_settings_edit(&mut self) {
        let Some(spec) = self.selected_credential_spec() else {
            self.settings.message =
                "This provider is auto-discovered; no pasted credential is needed.".into();
            return;
        };
        self.settings.editing = true;
        self.settings.input.clear();
        self.settings.message = format!("Paste {} below; input is hidden.", spec.kind.label());
    }

    fn save_settings_edit(&mut self) {
        let Some(spec) = self.selected_credential_spec() else {
            self.settings.editing = false;
            return;
        };
        let value = self.settings.input.trim().to_owned();
        if value.is_empty() {
            self.settings.message = "Nothing saved; press Esc to cancel.".into();
            return;
        }
        match credentials::set(spec.env_name, &value) {
            Ok(()) => {
                self.settings.message = format!(
                    "{} saved; refreshing its read-only collector…",
                    spec.provider_id
                );
                self.settings.editing = false;
                self.settings.input.clear();
                self.refresh_requested = true;
            }
            Err(error) => {
                self.settings.message = format!("save failed: {error}");
            }
        }
    }

    fn delete_settings_credential(&mut self) {
        let Some(spec) = self.selected_credential_spec() else {
            self.settings.message = "This provider has no editable credential.".into();
            return;
        };
        match credentials::remove(spec.env_name) {
            Ok(()) => {
                self.settings.message = format!("{} credential removed.", spec.provider_id);
                self.refresh_requested = true;
            }
            Err(error) => self.settings.message = format!("remove failed: {error}"),
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent) {
        if self.settings.editing {
            match key.code {
                KeyCode::Esc => {
                    self.settings.editing = false;
                    self.settings.input.clear();
                    self.settings.message = "Edit cancelled.".into();
                }
                KeyCode::Enter => self.save_settings_edit(),
                KeyCode::Backspace => {
                    self.settings.input.pop();
                }
                KeyCode::Char(character) if !character.is_control() => {
                    self.settings.input.push(character);
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('1') => {
                self.view = View::Limits;
                self.scroll = 0;
            }
            KeyCode::Char('2') => {
                self.view = View::Consumption;
                self.scroll = 0;
            }
            KeyCode::Char('3') => {}
            KeyCode::Tab => {
                self.view = View::Limits;
                self.scroll = 0;
            }
            KeyCode::BackTab => {
                self.view = View::Consumption;
                self.scroll = 0;
            }
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc | KeyCode::Char('p') | KeyCode::Char('s') => {
                self.view = View::Limits;
                self.scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings.selected = self.settings.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings.selected = (self.settings.selected + 1)
                    .min(self.settings.provider_ids.len().saturating_sub(1));
            }
            KeyCode::Enter => self.begin_settings_edit(),
            KeyCode::Char('d') => self.delete_settings_credential(),
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.refreshes = self.refreshes.saturating_add(1);
                self.last_refresh = Instant::now();
                self.limits_busy = true;
                self.refresh_requested = true;
            }
            _ => {}
        }
    }

    fn handle_paste(&mut self, pasted: &str) {
        if !self.settings.editing {
            return;
        }
        let clean = pasted.replace(['\r', '\n'], "");
        self.settings.input.push_str(clean.trim());
    }

    fn refresh_seconds(&self, args: &Args) -> Duration {
        Duration::from_secs(args.refresh.clamp(10, 1_800))
    }

    fn visible_providers(&self) -> Vec<&ProviderSnapshot> {
        let q = self.search_query.trim().to_ascii_lowercase();
        self.providers
            .iter()
            .filter(|provider| match self.filter {
                Filter::All => true,
                Filter::Attention => provider.availability != Availability::Available,
                Filter::Credits => provider.has_credits(),
                Filter::Quotas => !provider.has_credits(),
            })
            .filter(|p| {
                if q.is_empty() {
                    return true;
                }
                p.provider_id.to_ascii_lowercase().contains(&q)
                    || p.plan.to_ascii_lowercase().contains(&q)
                    || p.account_label.to_ascii_lowercase().contains(&q)
                    || provider_name(&p.provider_id).to_ascii_lowercase().contains(&q)
            })
            .collect()
    }
}

fn fixture_window(
    label: &str,
    kind: WindowKind,
    metric: WindowMetric,
    percent: Option<f64>,
    amount: Option<f64>,
    reset: Option<&str>,
    reset_ms: Option<i64>,
) -> LimitWindow {
    LimitWindow {
        label: label.into(),
        kind,
        metric,
        remaining_percent: percent,
        remaining_amount: amount,
        currency: Some("USD".into()),
        resets_at_ms: reset_ms,
        reset_text: reset.map(str::to_owned),
        estimated: metric.is_credit(),
    }
}

fn mock_consumption_report() -> usage::ConsumptionReport {
    let now = chrono::Local::now();
    let today_date = now.format("%Y-%m-%d").to_string();
    let yest_date = (now - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
    let d3_date = (now - chrono::Duration::days(3)).format("%Y-%m-%d").to_string();
    let d10_date = (now - chrono::Duration::days(10)).format("%Y-%m-%d").to_string();

    let records = vec![
        usage::UsageRecord {
            client: "Codex".into(),
            model_id: "gpt-4o".into(),
            provider_id: "codex".into(),
            session_id: "mock-sess-1".into(),
            date: today_date.clone(),
            timestamp: now.timestamp_millis() - 3600_000,
            tokens: usage::UsageTokens {
                input: 4_500_000,
                output: 320_000,
                cache_read: 85_000_000,
                cache_write: 500_000,
                reasoning: 0,
            },
            message_count: 42,
        },
        usage::UsageRecord {
            client: "Claude".into(),
            model_id: "claude-3-7-sonnet".into(),
            provider_id: "claude".into(),
            session_id: "mock-sess-2".into(),
            date: today_date.clone(),
            timestamp: now.timestamp_millis() - 7200_000,
            tokens: usage::UsageTokens {
                input: 2_100_000,
                output: 280_000,
                cache_read: 48_000_000,
                cache_write: 320_000,
                reasoning: 150_000,
            },
            message_count: 35,
        },
        usage::UsageRecord {
            client: "Antigravity Cli".into(),
            model_id: "gemini-2.5-flash".into(),
            provider_id: "antigravity".into(),
            session_id: "mock-sess-3".into(),
            date: today_date.clone(),
            timestamp: now.timestamp_millis() - 1800_000,
            tokens: usage::UsageTokens {
                input: 8_200_000,
                output: 240_000,
                cache_read: 180_000_000,
                cache_write: 1_200_000,
                reasoning: 0,
            },
            message_count: 58,
        },
        usage::UsageRecord {
            client: "Cursor".into(),
            model_id: "claude-3-5-sonnet".into(),
            provider_id: "cursor".into(),
            session_id: "mock-sess-4".into(),
            date: yest_date.clone(),
            timestamp: now.timestamp_millis() - 86400_000,
            tokens: usage::UsageTokens {
                input: 5_800_000,
                output: 650_000,
                cache_read: 250_000_000,
                cache_write: 1_500_000,
                reasoning: 0,
            },
            message_count: 80,
        },
        usage::UsageRecord {
            client: "Codex".into(),
            model_id: "o3-mini".into(),
            provider_id: "codex".into(),
            session_id: "mock-sess-5".into(),
            date: d3_date.clone(),
            timestamp: now.timestamp_millis() - 3 * 86400_000,
            tokens: usage::UsageTokens {
                input: 15_000_000,
                output: 1_800_000,
                cache_read: 620_000_000,
                cache_write: 4_000_000,
                reasoning: 1_200_000,
            },
            message_count: 150,
        },
        usage::UsageRecord {
            client: "Claude".into(),
            model_id: "claude-3-5-sonnet".into(),
            provider_id: "claude".into(),
            session_id: "mock-sess-6".into(),
            date: d10_date.clone(),
            timestamp: now.timestamp_millis() - 10 * 86400_000,
            tokens: usage::UsageTokens {
                input: 50_000_000,
                output: 4_500_000,
                cache_read: 1_800_000_000,
                cache_write: 12_000_000,
                reasoning: 0,
            },
            message_count: 420,
        },
    ];

    let snapshot = usage::UsageSnapshot {
        records,
        processing_time_ms: 12,
        tokscale_revision: "fixture".into(),
    };
    let pricing = token_monitor_core::pricing::PricingEngine::load_cached();
    usage::build_consumption_report(snapshot, &pricing, true)
}

fn fixtures() -> Vec<ProviderSnapshot> {
    let row = |account: &str,
               id: &str,
               plan: &str,
               availability: Availability,
               windows: Vec<LimitWindow>,
               hue: u8| ProviderSnapshot {
        provider_id: id.into(),
        account_key: account.into(),
        account_label: account.into(),
        plan: plan.into(),
        source: "fixture".into(),
        collected_at_ms: 0,
        source_health: SourceHealth::Connected,
        availability,
        windows,
        diagnostics: vec![],
        hue,
    };
    vec![
        row(
            "workspace-a",
            "modal",
            "",
            Availability::Available,
            vec![fixture_window(
                "credit~",
                WindowKind::Billing,
                WindowMetric::Credits,
                Some(8.7),
                Some(2.62),
                Some("4h 3m"),
                Some(14_580_000),
            )],
            214,
        ),
        row(
            "Codex · Plus",
            "codex",
            "",
            Availability::Available,
            vec![
                fixture_window(
                    "5h",
                    WindowKind::Session,
                    WindowMetric::Quota,
                    Some(90.0),
                    None,
                    Some("4h 16m"),
                    Some(15_360_000),
                ),
                fixture_window(
                    "7d",
                    WindowKind::Weekly,
                    WindowMetric::Quota,
                    Some(76.0),
                    None,
                    Some("6d 6h"),
                    Some(540_000_000),
                ),
            ],
            43,
        ),
        row(
            "Antigravity · Pro",
            "antigravity",
            "",
            Availability::Exhausted,
            vec![
                fixture_window(
                    "G5h",
                    WindowKind::Session,
                    WindowMetric::Quota,
                    Some(100.0),
                    None,
                    Some("5h 0m"),
                    Some(18_000_000),
                ),
                fixture_window(
                    "G7d",
                    WindowKind::Weekly,
                    WindowMetric::Quota,
                    Some(0.1),
                    None,
                    Some("1d 22h"),
                    Some(158_400_000),
                ),
                fixture_window(
                    "C7d",
                    WindowKind::Weekly,
                    WindowMetric::Quota,
                    Some(0.0),
                    None,
                    Some("1d 22h"),
                    Some(158_400_000),
                ),
            ],
            141,
        ),
        row(
            "Cursor · Free",
            "cursor",
            "",
            Availability::AgentBlocked,
            vec![
                fixture_window(
                    "Other",
                    WindowKind::Billing,
                    WindowMetric::Quota,
                    Some(97.0),
                    None,
                    Some("19h 23m"),
                    Some(69_780_000),
                ),
                fixture_window(
                    "Cursor",
                    WindowKind::Billing,
                    WindowMetric::Quota,
                    Some(100.0),
                    None,
                    Some("19h 23m"),
                    Some(69_780_000),
                ),
            ],
            75,
        ),
        row(
            "Claude · Pro",
            "claude",
            "",
            Availability::Available,
            vec![
                fixture_window(
                    "5h",
                    WindowKind::Session,
                    WindowMetric::Quota,
                    Some(100.0),
                    None,
                    None,
                    None,
                ),
                fixture_window(
                    "7d",
                    WindowKind::Weekly,
                    WindowMetric::Quota,
                    Some(66.0),
                    None,
                    Some("2d 9h"),
                    Some(205_200_000),
                ),
                fixture_window(
                    "credit",
                    WindowKind::Billing,
                    WindowMetric::Credits,
                    Some(72.3),
                    Some(72.27),
                    Some("2d 9h"),
                    Some(205_200_000),
                ),
            ],
            209,
        ),
        row(
            "Command Code · Go",
            "commandcode",
            "",
            Availability::Available,
            vec![
                fixture_window(
                    "5h",
                    WindowKind::Session,
                    WindowMetric::Quota,
                    Some(100.0),
                    None,
                    None,
                    None,
                ),
                fixture_window(
                    "7d",
                    WindowKind::Weekly,
                    WindowMetric::Quota,
                    Some(89.0),
                    None,
                    Some("4d 18h"),
                    Some(403_200_000),
                ),
                fixture_window(
                    "credit",
                    WindowKind::Billing,
                    WindowMetric::Credits,
                    Some(93.0),
                    Some(9.32),
                    Some("4d 18h"),
                    Some(403_200_000),
                ),
            ],
            220,
        ),
        row(
            "workspace-b",
            "modal",
            "",
            Availability::Available,
            vec![fixture_window(
                "credit~",
                WindowKind::Billing,
                WindowMetric::Credits,
                Some(100.0),
                Some(30.0),
                Some("29d 22h"),
                Some(2_588_400_000),
            )],
            214,
        ),
        row(
            "Grok · SuperGrok",
            "grok",
            "",
            Availability::Exhausted,
            vec![fixture_window(
                "Weekly",
                WindowKind::Weekly,
                WindowMetric::Quota,
                Some(0.0),
                None,
                Some("4d 6h"),
                Some(360_000_000),
            )],
            75,
        ),
        row(
            "OpenRouter · PAYG",
            "openrouter",
            "Pay-as-you-go",
            Availability::Available,
            vec![fixture_window(
                "credit",
                WindowKind::Billing,
                WindowMetric::Credits,
                Some(99.8),
                Some(9.98),
                None,
                None,
            )],
            141,
        ),
        row(
            "DeepSeek · PAYG",
            "deepseek",
            "Pay-as-you-go",
            Availability::Available,
            vec![fixture_window(
                "balance",
                WindowKind::Billing,
                WindowMetric::Credits,
                None,
                Some(8.71),
                None,
                None,
            )],
            75,
        ),
        row(
            "Vast.ai · Cloud GPU",
            "vast",
            "",
            Availability::Available,
            vec![fixture_window(
                "credit",
                WindowKind::Billing,
                WindowMetric::Credits,
                None,
                Some(5.68),
                None,
                None,
            )],
            220,
        ),
    ]
}

const ACCOUNT_COLORS: [Color; 8] = [
    Color::Rgb(96, 198, 255),
    Color::Rgb(77, 220, 177),
    Color::Rgb(190, 145, 255),
    Color::Rgb(255, 190, 80),
    Color::Rgb(80, 211, 226),
    Color::Rgb(255, 125, 165),
    Color::Rgb(140, 165, 255),
    Color::Rgb(125, 225, 115),
];

fn palette(hue: u8) -> Color {
    ACCOUNT_COLORS[(hue as usize / 30) % ACCOUNT_COLORS.len()]
}

#[allow(dead_code)]
fn format_percent(value: f64) -> String {
    if value.fract().abs() > 0.01 {
        format!("{value:.1}%")
    } else {
        format!("{value:.0}%")
    }
}

fn format_tokens(value: i64) -> String {
    let value = value.max(0) as f64;
    if value >= 1_000_000_000.0 {
        format!("{:.2}B", value / 1_000_000_000.0)
    } else if value >= 1_000_000.0 {
        format!("{:.2}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else {
        format!("{value:.0}")
    }
}

fn meter(percent: f64, width: usize) -> String {
    let clamped = percent.clamp(0.0, 100.0);
    let mut filled = ((clamped / 100.0) * width as f64).round() as usize;
    if clamped > 0.0 && filled == 0 {
        filled = 1;
    }
    filled = filled.min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn str_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let current_width = str_width(text);
    if current_width <= width {
        let mut value = text.to_owned();
        value.push_str(&" ".repeat(width - current_width));
        return value;
    }
    if width == 1 {
        return "…".to_owned();
    }
    let target_width = width - 1;
    let mut truncated = String::new();
    let mut acc = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc + w > target_width {
            break;
        }
        truncated.push(ch);
        acc += w;
    }
    truncated.push('…');
    let final_w = str_width(&truncated);
    if final_w < width {
        truncated.push_str(&" ".repeat(width - final_w));
    }
    truncated
}

#[allow(dead_code)]
fn window_text(window: &LimitWindow, wide: bool) -> String {
    let percent = window
        .remaining_percent
        .map(format_percent)
        .unwrap_or_else(|| "—".into());
    let amount = window
        .remaining_amount
        .map(|value| match window.currency.as_deref().unwrap_or("USD") {
            "USD" | "" => format!("${value:.2}"),
            currency => format!("{currency} {value:.2}"),
        })
        .unwrap_or_default();
    if wide && !window.metric.is_credit() && window.remaining_percent.is_some() {
        format!(
            "{} {:>5} [{}]",
            window.label,
            percent,
            meter(window.remaining_percent.unwrap_or(0.0), 8)
        )
    } else if wide && window.metric.is_credit() && !amount.is_empty() {
        let value = if percent == "—" {
            amount
        } else {
            format!("{amount} {percent}")
        };
        if window.remaining_percent.is_some() {
            format!(
                "{value} {}",
                meter(window.remaining_percent.unwrap_or(0.0), 5)
            )
        } else {
            value
        }
    } else if amount.is_empty() {
        format!("{} {}", window.label, percent)
    } else {
        if percent == "—" {
            format!("{} {}", window.label, amount)
        } else {
            format!("{} {}", amount, percent)
        }
    }
}

fn provider_name(provider_id: &str) -> &str {
    token_monitor_core::provider_registry::display_name(provider_id)
}

fn mask_email(value: &str) -> String {
    let Some((local, domain)) = value.split_once('@') else {
        return value.to_owned();
    };
    let local = local.trim();
    if local.is_empty() || domain.trim().is_empty() {
        return value.to_owned();
    }
    let first = local.chars().next().unwrap_or('*');
    format!("{first}***@{}", domain.trim())
}

fn mask_identifier(value: &str) -> String {
    let value = value.trim();
    if value.contains('@') {
        return mask_email(value);
    }
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() >= 5
        && chars
            .first()
            .is_some_and(|character| character.is_ascii_lowercase())
    {
        let suffix = chars.iter().rev().take(2).rev().collect::<String>();
        return format!("{}***{suffix}", chars[0]);
    }
    value.to_owned()
}

fn masked_account_label(provider: &ProviderSnapshot) -> String {
    let raw = provider.account_label.trim();
    if raw.is_empty() {
        return String::new();
    }
    let name = provider_name(&provider.provider_id);
    let plan = provider.plan.trim();
    // Fixture and legacy labels sometimes already contain the provider and
    // plan. Keep those human-readable and mask only the identity segment.
    let parts = raw.split(" · ").collect::<Vec<_>>();
    if parts.len() > 1 && parts[0].eq_ignore_ascii_case(name) {
        let mut masked = parts[..parts.len() - 1]
            .iter()
            .map(|part| (*part).to_owned())
            .collect::<Vec<_>>();
        masked.push(mask_identifier(parts[parts.len() - 1]));
        return masked.join(" · ");
    }
    if raw.eq_ignore_ascii_case(name) || (!plan.is_empty() && raw.eq_ignore_ascii_case(plan)) {
        return raw.to_owned();
    }
    // Capitalized product labels (SuperGrok, Cloud GPU, Ollama, …) are not
    // account identifiers. Lowercase profile names and email addresses are.
    if raw.contains('@')
        || raw
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_lowercase())
    {
        mask_identifier(raw)
    } else {
        raw.to_owned()
    }
}

fn provider_title_base(provider: &ProviderSnapshot) -> String {
    let name = provider_name(&provider.provider_id);
    let plan = if provider.plan.trim().is_empty() {
        provider
            .account_label
            .split(" · ")
            .nth(1)
            .filter(|candidate| {
                matches!(
                    candidate.to_ascii_lowercase().as_str(),
                    "free"
                        | "plus"
                        | "pro"
                        | "go"
                        | "team"
                        | "supergrok"
                        | "payg"
                        | "pay-as-you-go"
                        | "cloud gpu"
                )
            })
            .map(display_plan)
            .unwrap_or_default()
    } else {
        display_plan(provider.plan.trim())
    };
    if plan.is_empty() || plan.eq_ignore_ascii_case(name) {
        name.to_owned()
    } else {
        format!("{name} · {plan}")
    }
}

fn display_plan(plan: &str) -> String {
    match plan.to_ascii_lowercase().as_str() {
        "free" => "Free".into(),
        "plus" => "Plus".into(),
        "pro" => "Pro".into(),
        "team" | "teams" => "Team".into(),
        "pay-as-you-go" | "pay as you go" | "payg" => "Pay-as-you-go".into(),
        _ => plan.to_owned(),
    }
}

#[allow(dead_code)]
fn provider_identity(provider: &ProviderSnapshot, show_account: bool) -> Option<String> {
    let raw = provider.account_label.trim();
    if raw.is_empty() {
        return None;
    }
    let name = provider_name(&provider.provider_id);
    let plan = provider.plan.trim();
    let candidate = if raw
        .to_ascii_lowercase()
        .starts_with(&name.to_ascii_lowercase())
    {
        raw.split(" · ").last().unwrap_or(raw).trim()
    } else {
        raw
    };
    if candidate.is_empty()
        || candidate.eq_ignore_ascii_case(name)
        || (!plan.is_empty() && candidate.eq_ignore_ascii_case(plan))
        || candidate.eq_ignore_ascii_case("payg")
        || candidate.eq_ignore_ascii_case("cloud gpu")
        || candidate.eq_ignore_ascii_case("free")
    {
        return None;
    }
    let value = if show_account {
        candidate.to_owned()
    } else {
        mask_identifier(candidate)
    };
    Some(value)
}

#[allow(dead_code)]
fn provider_titles(providers: &[&ProviderSnapshot], show_account: bool) -> Vec<String> {
    let bases = providers
        .iter()
        .map(|provider| provider_title_base(provider))
        .collect::<Vec<_>>();
    let mut counts = HashMap::<String, usize>::new();
    for base in &bases {
        *counts.entry(base.clone()).or_default() += 1;
    }
    providers
        .iter()
        .zip(bases)
        .map(|(provider, base)| {
            if counts.get(&base).copied().unwrap_or(0) > 1 {
                if let Some(identity) = provider_identity(provider, show_account) {
                    return format!("{base} · {identity}");
                }
            }
            base
        })
        .collect()
}

#[allow(dead_code)]
fn status_label(provider: &ProviderSnapshot) -> &'static str {
    if provider.availability == Availability::Available
        && provider.source_health == SourceHealth::Stale
    {
        return "stale";
    }
    if provider.availability != Availability::Unknown {
        return provider.availability.label();
    }
    provider.source_health.label()
}

fn row_dimmed(provider: &ProviderSnapshot) -> bool {
    provider.availability.dimmed() || provider.source_health != SourceHealth::Connected
}

fn status_style_for(provider: &ProviderSnapshot) -> Style {
    let color = match (provider.availability, provider.source_health) {
        (Availability::Exhausted, _) => RED,
        (Availability::AgentBlocked, _) => YELLOW,
        (Availability::Available, SourceHealth::Connected) => GREEN,
        (Availability::Available, _) => YELLOW,
        (_, SourceHealth::Unauthorized | SourceHealth::Error) => RED,
        _ => GREY,
    };
    let mut style = Style::default().fg(color);
    if row_dimmed(provider) {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

fn reset_text(window: &LimitWindow) -> String {
    if let Some(timestamp) = window.resets_at_ms {
        let now = chrono::Utc::now().timestamp_millis();
        // Fixtures and a few legacy adapters carry a human countdown beside a
        // non-absolute placeholder timestamp. Prefer that trusted text instead
        // of rendering an accidental `0m` countdown.
        if timestamp <= now {
            if let Some(text) = &window.reset_text {
                return compact_reset_description(text);
            }
        }
        let remaining = (timestamp - now).max(0) / 1000;
        let days = remaining / 86_400;
        let hours = (remaining % 86_400) / 3_600;
        let minutes = (remaining % 3_600) / 60;
        return if days > 0 {
            format!("{days}d {hours}h")
        } else if hours > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{minutes}m")
        };
    }
    window
        .reset_text
        .as_deref()
        .map(compact_reset_description)
        .unwrap_or_else(|| "—".into())
}

fn compact_reset_description(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "monthly grant reset" => "monthly".into(),
        "weekly quota reset" => "weekly".into(),
        "daily quota reset" => "daily".into(),
        _ => value.trim().to_owned(),
    }
}

fn age_text(collected_at_ms: i64) -> String {
    if collected_at_ms <= 0 {
        return "—".into();
    }
    let seconds = (chrono::Utc::now().timestamp_millis() - collected_at_ms).max(0) / 1000;
    if seconds < 60 {
        "now".into()
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn header_lines(app: &App, width: u16, refresh_seconds: u64) -> Vec<Line<'static>> {
    let visible = app.visible_providers().len();
    let total = app.providers.len();
    let connected = app
        .providers
        .iter()
        .filter(|row| {
            row.availability == Availability::Available
                && row.source_health == SourceHealth::Connected
        })
        .count();
    let attention = app
        .providers
        .iter()
        .filter(|row| {
            row.availability != Availability::Available
                || row.source_health != SourceHealth::Connected
        })
        .count();
    let wallets = app
        .providers
        .iter()
        .filter(|row| row.has_credits() || row.payg())
        .count();
    let updated_at = app
        .providers
        .iter()
        .map(|row| row.collected_at_ms)
        .max()
        .unwrap_or_default();

    let mut first_row = vec![
        Span::styled(
            "TOKEN MONITOR",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];

    let t1_style = if app.view == View::Limits {
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(GREY)
    };
    let t2_style = if app.view == View::Consumption {
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(GREY)
    };
    let t3_style = if app.view == View::Settings {
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(GREY)
    };

    first_row.push(Span::styled("[1] Limits", t1_style));
    first_row.push(Span::raw(" "));
    first_row.push(Span::styled("[2] Ledger", t2_style));
    first_row.push(Span::raw(" "));
    first_row.push(Span::styled("[3] Config", t3_style));

    let is_refreshing = match app.view {
        View::Consumption => app.usage_busy,
        View::Limits | View::Settings => app.limits_busy,
    } || app.last_refresh.elapsed() < Duration::from_millis(1500);

    let refresh_indicator = if is_refreshing {
        Span::styled(
            "↻ refreshing...",
            Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            format!("↻ {}s", refresh_seconds.clamp(10, 1800)),
            Style::default().fg(GREY),
        )
    };

    let summary_spans = vec![
        Span::styled(format!("● {connected} ready"), Style::default().fg(GREEN)),
        Span::raw(" · "),
        Span::styled(
            format!("▲ {attention} attention"),
            Style::default().fg(if attention > 0 { YELLOW } else { GREY }),
        ),
        Span::raw(" · "),
        Span::styled(format!("◈ {wallets} wallets"), Style::default().fg(BLUE)),
        Span::raw(" · "),
        refresh_indicator.clone(),
    ];

    let brand_len: usize = first_row.iter().map(|s| str_width(&s.content)).sum();
    let summary_len: usize = summary_spans.iter().map(|s| str_width(&s.content)).sum();
    let refresh_len: usize = str_width(&refresh_indicator.content);
    let pad_full = (width as usize).saturating_sub(brand_len + summary_len);
    let pad_compact = (width as usize).saturating_sub(brand_len + refresh_len);
    if width >= 105 && pad_full > 1 {
        first_row.push(Span::raw(" ".repeat(pad_full)));
        first_row.extend(summary_spans);
    } else if width >= 55 && pad_compact > 1 {
        first_row.push(Span::raw(" ".repeat(pad_compact)));
        first_row.push(refresh_indicator);
    }

    let mode = if is_refreshing {
        "native collector · ↻ refreshing..."
    } else if app.live {
        "native collector · live read-only"
    } else if app.refreshes == 0 {
        "native collector · fixture"
    } else {
        "native collector · refreshed"
    };

    let mut subtitle_spans = vec![
        Span::styled(
            format!("{visible}/{total} shown · updated {} · ", age_text(updated_at)),
            Style::default().fg(DIM_GREY),
        ),
    ];
    if is_refreshing {
        subtitle_spans.push(Span::styled(
            "↻ refreshing...",
            Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
        ));
    } else {
        subtitle_spans.push(Span::styled(mode, Style::default().fg(DIM_GREY)));
    }

    vec![
        Line::from(first_row),
        Line::from(subtitle_spans),
    ]
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
struct Columns {
    account: usize,
    status: usize,
    limits: usize,
    credits: usize,
    reset: usize,
    source: usize,
    total: usize,
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
fn columns_total(
    account: usize,
    status: usize,
    limits: usize,
    credits: usize,
    reset: usize,
    source: usize,
    gap: usize,
    column_count: usize,
) -> usize {
    account + status + limits + credits + reset + source + gap * column_count.saturating_sub(1)
}

#[allow(dead_code)]
fn columns(providers: &[&ProviderSnapshot], titles: &[String], width: u16) -> Columns {
    let width = width as usize;
    let gap = 2usize;
    let mut account = titles
        .iter()
        .map(|title| title.chars().count())
        .max()
        .unwrap_or(8)
        .saturating_add(2)
        .clamp(18, 34);
    let mut status = providers
        .iter()
        .map(|provider| {
            format!(
                "{} {}",
                provider.availability.marker(),
                status_label(provider)
            )
            .chars()
            .count()
        })
        .max()
        .unwrap_or(6)
        .clamp(6, 16);
    let has_credits = providers.iter().any(|provider| {
        provider
            .windows
            .iter()
            .any(|window| window.metric.is_credit())
    });
    let has_resets = providers
        .iter()
        .flat_map(|provider| provider.windows.iter())
        .any(|window| window.resets_at_ms.is_some() || window.reset_text.is_some());
    let has_source = width >= 132 && providers.iter().any(|provider| !provider.source.is_empty());
    let mut credits = if has_credits { 12 } else { 0 };
    let mut reset = if has_resets { 8 } else { 0 };
    let mut source = if has_source { 8 } else { 0 };
    if credits > 0 {
        credits = providers
            .iter()
            .flat_map(|provider| provider.windows.iter())
            .filter(|window| window.metric.is_credit())
            .map(|window| window_text(window, true).chars().count())
            .max()
            .unwrap_or(12)
            .clamp(12, 20);
    }
    if reset > 0 {
        reset = providers
            .iter()
            .flat_map(|provider| provider.windows.iter())
            .map(reset_text)
            .map(|value| value.chars().count())
            .max()
            .unwrap_or(8)
            .clamp(8, 10);
    }
    if source > 0 {
        source = providers
            .iter()
            .map(|provider| source_display(provider))
            .map(|value| value.chars().count())
            .max()
            .unwrap_or(8)
            .clamp(8, 18);
    }
    let quota_min = if width >= 120 { 24 } else { 18 };
    let quota_max = if width >= 140 {
        56
    } else if width >= 110 {
        48
    } else {
        34
    };
    let full_label_width = providers
        .iter()
        .flat_map(|provider| ordered_windows(provider))
        .filter(|window| !window.metric.is_credit())
        .map(|window| short_window_label(&window.label, quota_max).chars().count())
        .max()
        .unwrap_or(6);
    let label_width = if quota_max < 24 {
        full_label_width.min(6)
    } else {
        full_label_width
    };
    let preferred_limits = providers
        .iter()
        .flat_map(|provider| ordered_windows(provider))
        .filter(|window| !window.metric.is_credit())
        .map(|window| {
            quota_summary(window, quota_max, label_width)
                .chars()
                .count()
        })
        .max()
        .unwrap_or(quota_min)
        .clamp(quota_min, quota_max);
    let mut columns =
        3 + usize::from(credits > 0) + usize::from(reset > 0) + usize::from(source > 0);
    // Optional columns are less important than preserving a readable quota
    // cell. Drop them in the same order as the JS renderer when a pane is tight.
    while columns_total(
        account, status, quota_min, credits, reset, source, gap, columns,
    ) > width
        && source > 0
    {
        source = 0;
        columns -= 1;
    }
    while columns_total(
        account, status, quota_min, credits, reset, source, gap, columns,
    ) > width
        && reset > 0
    {
        reset = 0;
        columns -= 1;
    }
    while columns_total(
        account, status, quota_min, credits, reset, source, gap, columns,
    ) > width
        && credits > 0
    {
        credits = 0;
        columns -= 1;
    }
    if columns_total(
        account, status, quota_min, credits, reset, source, gap, columns,
    ) > width
    {
        account = 18;
        status = 6;
    }
    let available_limits = width.saturating_sub(columns_total(
        account, status, 0, credits, reset, source, gap, columns,
    ));
    let limits = preferred_limits.min(available_limits).max(1);
    let total = columns_total(
        account, status, limits, credits, reset, source, gap, columns,
    );
    Columns {
        account,
        status,
        limits,
        credits,
        reset,
        source,
        total,
    }
}

#[allow(dead_code)]
fn ordered_windows(provider: &ProviderSnapshot) -> Vec<&LimitWindow> {
    // Preserve each adapter's deliberate family ordering. In particular,
    // Antigravity's Gemini 5h/Gemini 7d/Claude 7d sequence is more legible
    // than re-sorting unrelated families by percentage. Provider ordering
    // already carries the burn-first urgency signal.
    provider.windows.iter().collect()
}

#[allow(dead_code)]
fn short_window_label(label: &str, width: usize) -> String {
    if width >= 26 {
        return label.to_owned();
    }
    let lower = label.to_ascii_lowercase();
    if lower == "cursor models" {
        "Cursor".into()
    } else if lower == "other models" {
        "Other".into()
    } else if lower.starts_with("gemini ") {
        format!("G{}", &label[7..])
    } else if lower.starts_with("claude/gpt ") {
        format!("C{}", &label[11..])
    } else if lower.starts_with("claude ") {
        format!("C{}", &label[7..])
    } else if lower == "requests" {
        "Req".into()
    } else {
        label.to_owned()
    }
}

#[allow(dead_code)]
fn quota_summary(window: &LimitWindow, width: usize, label_width: usize) -> String {
    let label = short_window_label(&window.label, width);
    let percent = window
        .remaining_percent
        .map(format_percent)
        .unwrap_or_else(|| "—".into());
    let base = format!("{label:<label_width$} {percent:>5}");
    let Some(value) = window.remaining_percent else {
        return base;
    };
    let available = width.saturating_sub(base.chars().count() + 3);
    if available >= 3 {
        let meter_text = if available >= 6 {
            format!(" [{}]", meter(value, available.min(8)))
        } else {
            format!(" {}", meter(value, available))
        };
        format!("{base}{meter_text}")
    } else {
        base
    }
}

#[allow(dead_code)]
fn credit_summary(window: &LimitWindow, width: usize) -> String {
    let amount =
        window
            .remaining_amount
            .map(|value| match window.currency.as_deref().unwrap_or("USD") {
                "USD" | "" => format!("${value:.2}"),
                currency => format!("{currency} {value:.2}"),
            });
    let percent = window.remaining_percent.map(format_percent);
    let value = match (amount, percent) {
        (Some(amount), Some(percent)) => format!("{amount} {percent}"),
        (Some(amount), None) => amount,
        (None, Some(percent)) => format!("{} {percent}", window.label),
        (None, None) => window.label.clone(),
    };
    if width >= value.chars().count() + 4 && window.remaining_percent.is_some() {
        let available = width.saturating_sub(value.chars().count() + 1).min(5);
        format!(
            "{value} {}",
            meter(window.remaining_percent.unwrap_or(0.0), available)
        )
    } else {
        value
    }
}

#[allow(dead_code)]
fn pack_texts(values: Vec<String>, width: usize) -> Vec<String> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for value in values {
        let candidate = if current.is_empty() {
            value.clone()
        } else {
            format!("{current}  {value}")
        };
        if !current.is_empty() && candidate.chars().count() > width {
            lines.push(current);
            current = value;
        } else if value.chars().count() > width && current.is_empty() {
            lines.push(fit(&value, width));
        } else {
            current = candidate;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[allow(dead_code)]
fn next_reset(provider: &ProviderSnapshot) -> Option<String> {
    let blocked_durable = provider.availability != Availability::Available;
    let mut candidates = provider
        .windows
        .iter()
        .filter(|window| {
            if blocked_durable {
                !window.metric.is_credit()
                    && window.kind.durable()
                    && window.effectively_exhausted()
            } else {
                true
            }
        })
        .filter_map(|window| window.resets_at_ms.map(|reset| (reset, window)))
        .collect::<Vec<_>>();
    // An agent-blocked row may only have a billing/credit reset, and a
    // partially populated adapter may not expose an explicit durable window.
    // Fall back to the earliest timestamp rather than hiding the reset.
    if candidates.is_empty() {
        candidates = provider
            .windows
            .iter()
            .filter_map(|window| window.resets_at_ms.map(|reset| (reset, window)))
            .collect();
    }
    candidates
        .into_iter()
        .min_by_key(|(reset, _)| *reset)
        .map(|(_, window)| reset_text(window))
}

#[allow(dead_code)]
fn source_age(provider: &ProviderSnapshot) -> String {
    if provider.collected_at_ms <= 0 {
        return "—".into();
    }
    let seconds = (chrono::Utc::now().timestamp_millis() - provider.collected_at_ms).max(0) / 1000;
    if seconds < 60 {
        "now".into()
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

#[allow(dead_code)]
fn source_display(provider: &ProviderSnapshot) -> String {
    let source = if provider.source.trim().is_empty() {
        "—"
    } else {
        provider.source.trim()
    };
    let age = source_age(provider);
    if age == "—" {
        source.to_owned()
    } else {
        format!("{source} · {age}")
    }
}

#[allow(dead_code)]
fn compact_metric(provider: &ProviderSnapshot, width: usize) -> String {
    let windows = ordered_windows(provider);
    let quota_windows = windows
        .iter()
        .filter(|window| window.remaining_percent.is_some())
        .copied()
        .collect::<Vec<_>>();
    let quota_windows = if quota_windows.is_empty() {
        windows
            .iter()
            .filter(|window| !window.metric.is_credit())
            .copied()
            .collect::<Vec<_>>()
    } else {
        quota_windows
    };
    let label_width = if width >= 58 { width } else { 20 };
    let mut values = quota_windows
        .iter()
        .filter(|window| !window.metric.is_credit())
        .map(|window| {
            let label = short_window_label(&window.label, label_width);
            let percent = window
                .remaining_percent
                .map(format_percent)
                .unwrap_or_else(|| "—".into());
            if width >= 58 {
                format!(
                    "{} {} [{}]",
                    label,
                    percent,
                    meter(window.remaining_percent.unwrap_or(0.0), 5)
                )
            } else {
                format!("{label} {percent}")
            }
        })
        .collect::<Vec<_>>();
    if values.is_empty() {
        values = windows
            .iter()
            .filter(|window| window.metric.is_credit())
            .map(|window| window_text(window, width >= 58))
            .collect();
    }
    if values.is_empty() {
        values.push("no quota data".into());
    }
    let max_values = if width >= 42 { 2 } else { 1 };
    let omitted = values.len().saturating_sub(max_values);
    values.truncate(max_values);
    let status_prefix = if provider.availability != Availability::Available
        || provider.source_health != SourceHealth::Connected
    {
        format!("{} · ", status_label(provider))
    } else {
        String::new()
    };
    let reset_suffix = next_reset(provider)
        .map(|reset| format!("  ↻{reset}"))
        .unwrap_or_default();
    let mut shown = values;
    let mut hidden = omitted;
    loop {
        let marker = if hidden > 0 {
            Some(if width < 48 {
                format!("+{hidden}")
            } else {
                format!("+{hidden} more")
            })
        } else {
            None
        };
        let mut segments = shown.clone();
        if let Some(marker) = marker {
            segments.push(marker);
        }
        let value = format!("{}{}{}", status_prefix, segments.join("  "), reset_suffix);
        if str_width(&value) <= width {
            return value;
        }
        // Preserve a complete window boundary and the reset by replacing the
        // last visible window with a compact hidden-count marker.
        if shown.len() > 1 {
            shown.pop();
            hidden += 1;
            continue;
        }
        // If the row still cannot fit, drop the reset only as a final compact
        // fallback; the detailed view retains every reset value.
        if !reset_suffix.is_empty() {
            let value = format!("{}{}", status_prefix, segments.join("  "));
            if str_width(&value) <= width {
                return value;
            }
        }
        return fit(&value, width);
    }
}

#[derive(Clone, Debug)]
struct TemporalUsageBucket {
    label: &'static str,
    tokens: i64,
    input: i64,
    output: i64,
    cache: i64,
    api_usd: f64,
    records: usize,
}
fn normalize_client_id(client: &str) -> String {
    let s = client.trim().to_ascii_lowercase();
    if s.contains("codex") || s.contains("openai") {
        "Codex".to_owned()
    } else if s.contains("claude") || s.contains("anthropic") {
        "Claude".to_owned()
    } else if s.contains("antigravity-cli") || s.contains("antigravity cli") || s == "antigravity_cli" {
        "Antigravity Cli".to_owned()
    } else if s.contains("antigravity") || s.contains("google") {
        "Antigravity".to_owned()
    } else if s.contains("opencode") {
        "OpenCode".to_owned()
    } else if s.contains("grok") || s.contains("xai") {
        "Grok".to_owned()
    } else if s.contains("cursor") {
        "Cursor".to_owned()
    } else if s.is_empty() {
        "Unknown".to_owned()
    } else {
        let mut chars = s.chars();
        match chars.next() {
            Some(f) => format!("{}{}", f.to_ascii_uppercase(), chars.as_str()),
            None => "Unknown".to_owned(),
        }
    }
}

fn client_brand_color(client: &str) -> Color {
    let s = client.trim().to_ascii_lowercase();
    if s.contains("codex") || s.contains("openai") {
        Color::Rgb(16, 163, 127) // OpenAI Jade #10A37F
    } else if s.contains("claude") || s.contains("anthropic") {
        Color::Rgb(217, 119, 87) // Anthropic Terracotta #D97757
    } else if s.contains("antigravity") || s.contains("google") {
        Color::Rgb(66, 133, 244) // Google Azure #4285F4
    } else if s.contains("cursor") {
        Color::Rgb(0, 180, 216)  // Cursor Cyan #00B4D8
    } else if s.contains("grok") || s.contains("xai") {
        Color::Rgb(210, 215, 225) // xAI Silver #D2D7E1
    } else if s.contains("opencode") || s.contains("vast") {
        Color::Rgb(255, 140, 50)  // Vast Sunset Orange #FF8C32
    } else {
        Color::Rgb(125, 207, 255) // Default Cyan
    }
}

fn colored_meter(percent: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    let filled = ((percent.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled("░".repeat(empty), Style::default().fg(DIM_GREY)),
    ]
}

fn local_today_str() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn local_today_start_ms() -> i64 {
    chrono::Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|naive| naive.and_local_timezone(chrono::Local).earliest())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_default()
}

fn record_in_window(rec: &usage::UsageRecord, window: LedgerWindow, now_ms: i64, today_str: &str) -> bool {
    let day_ms = 86_400_000i64;
    match window {
        LedgerWindow::AllTime => true,
        LedgerWindow::Today => {
            let today_start_ms = local_today_start_ms();
            rec.date == today_str || (today_start_ms > 0 && rec.timestamp >= today_start_ms)
        }
        LedgerWindow::Last24h => rec.timestamp >= now_ms.saturating_sub(day_ms),
        LedgerWindow::Last7d => rec.timestamp >= now_ms.saturating_sub(7 * day_ms),
        LedgerWindow::Last30d => rec.timestamp >= now_ms.saturating_sub(30 * day_ms),
    }
}

#[derive(Clone)]
struct ConsumptionRow {
    client: String,
    provider: String,
    records: usize,
    tokens: usage::UsageTokens,
    api_value_usd: f64,
    api_rows: usize,
    priced_tokens: i64,
    unpriced_tokens: i64,
    partial_rows: usize,
    unknown_rows: usize,
}

impl ConsumptionRow {
    fn new(client: &str, provider: &str) -> Self {
        Self {
            client: client.to_owned(),
            provider: provider.to_owned(),
            records: 0,
            tokens: usage::UsageTokens::default(),
            api_value_usd: 0.0,
            api_rows: 0,
            priced_tokens: 0,
            unpriced_tokens: 0,
            partial_rows: 0,
            unknown_rows: 0,
        }
    }

    fn token_total(&self) -> i64 {
        self.tokens.reported_total_without_reasoning()
    }

    fn coverage(&self) -> Option<f64> {
        let total = self.token_total();
        (total > 0).then_some((self.priced_tokens as f64 / total as f64).clamp(0.0, 1.0))
    }

    fn api_text(&self) -> String {
        if self.api_rows == 0 {
            return "—".into();
        }
        let incomplete = self.partial_rows > 0 || self.unknown_rows > 0 || self.unpriced_tokens > 0;
        format!(
            "${:.2}{}",
            self.api_value_usd,
            if incomplete { "~" } else { "" }
        )
    }
}

fn consumption_label(row: &ConsumptionRow) -> String {
    let client = row.client.trim();
    if token_monitor_core::provider_registry::ALL_PROVIDER_IDS.contains(&client) {
        return token_monitor_core::provider_registry::display_name(client).to_owned();
    }
    let source = if client.is_empty() {
        row.provider.trim()
    } else {
        client
    };
    if source.is_empty() {
        return "Unknown".into();
    }
    source
        .split(['-', '_', '.'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[allow(dead_code)]
fn consumption_hue(row: &ConsumptionRow) -> u8 {
    row.client
        .bytes()
        .chain(row.provider.bytes())
        .fold(0u8, |sum, value| sum.wrapping_add(value))
}

fn consumption_rows(
    report: &usage::ConsumptionReport,
    metric: ConsumptionMetric,
    window: LedgerWindow,
    query: &str,
) -> Vec<ConsumptionRow> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let today_str = local_today_str();
    let mut grouped: HashMap<String, ConsumptionRow> = HashMap::new();
    let pricing = token_monitor_core::pricing::PricingEngine::load_cached();

    let q = query.trim().to_ascii_lowercase();
    for (index, record) in report.snapshot.records.iter().enumerate() {
        if !record_in_window(record, window, now_ms, &today_str) {
            continue;
        }
        let client_norm = normalize_client_id(&record.client);
        if !q.is_empty() {
            let m1 = client_norm.to_ascii_lowercase().contains(&q);
            let m2 = record.model_id.to_ascii_lowercase().contains(&q);
            let m3 = record.provider_id.to_ascii_lowercase().contains(&q);
            if !m1 && !m2 && !m3 {
                continue;
            }
        }
        let row = grouped
            .entry(client_norm.clone())
            .or_insert_with(|| ConsumptionRow::new(&client_norm, &record.provider_id));
        row.records += 1;
        row.tokens.add_assign(&record.tokens);
        let cached_quote = report.quotes.as_ref().and_then(|quotes| quotes.get(index));
        let val = cached_quote
            .and_then(|q| q.value_usd)
            .unwrap_or_else(|| pricing.quote(record).value_usd.unwrap_or(0.0));
        if val > 0.0 {
            row.api_value_usd += val;
            row.api_rows += 1;
        }
        if let Some(quote) = cached_quote {
            row.priced_tokens = row.priced_tokens.saturating_add(quote.priced_tokens);
            row.unpriced_tokens = row.unpriced_tokens.saturating_add(quote.unpriced_tokens);
            match quote.status {
                token_monitor_core::pricing::PricingStatus::Exact => {}
                token_monitor_core::pricing::PricingStatus::Partial => row.partial_rows += 1,
                token_monitor_core::pricing::PricingStatus::Unknown => row.unknown_rows += 1,
            }
        } else {
            let quote = pricing.quote(record);
            row.priced_tokens = row.priced_tokens.saturating_add(quote.priced_tokens);
            row.unpriced_tokens = row.unpriced_tokens.saturating_add(quote.unpriced_tokens);
            match quote.status {
                token_monitor_core::pricing::PricingStatus::Exact => {}
                token_monitor_core::pricing::PricingStatus::Partial => row.partial_rows += 1,
                token_monitor_core::pricing::PricingStatus::Unknown => row.unknown_rows += 1,
            }
        }
    }
    let mut rows = grouped.into_values().collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        match metric {
            ConsumptionMetric::Tokens => right.token_total().cmp(&left.token_total()),
            ConsumptionMetric::ApiEquivalent => right
                .api_value_usd
                .partial_cmp(&left.api_value_usd)
                .unwrap_or(std::cmp::Ordering::Equal),
        }
    });
    rows
}

fn compute_temporal_usage(report: &usage::ConsumptionReport) -> Vec<TemporalUsageBucket> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let today_date = local_today_str();
    let today_start_ms = local_today_start_ms();
    let h24_ms = now_ms - 24 * 3600 * 1000;
    let d7_ms = now_ms - 7 * 86400 * 1000;
    let d30_ms = now_ms - 30 * 86400 * 1000;

    let pricing = token_monitor_core::pricing::PricingEngine::load_cached();
    let mut today = TemporalUsageBucket { label: "Today", tokens: 0, input: 0, output: 0, cache: 0, api_usd: 0.0, records: 0 };
    let mut h24 = TemporalUsageBucket { label: "Last 24h", tokens: 0, input: 0, output: 0, cache: 0, api_usd: 0.0, records: 0 };
    let mut d7 = TemporalUsageBucket { label: "Last 7d", tokens: 0, input: 0, output: 0, cache: 0, api_usd: 0.0, records: 0 };
    let mut d30 = TemporalUsageBucket { label: "Last 30d", tokens: 0, input: 0, output: 0, cache: 0, api_usd: 0.0, records: 0 };
    let mut all = TemporalUsageBucket { label: "All Time", tokens: 0, input: 0, output: 0, cache: 0, api_usd: 0.0, records: 0 };

    for (index, record) in report.snapshot.records.iter().enumerate() {
        let tok = record.tokens.reported_total_without_reasoning();
        let inp = record.tokens.input;
        let out = record.tokens.output;
        let cch = record.tokens.cache_read.saturating_add(record.tokens.cache_write);
        let val = if let Some(quotes) = report.quotes.as_ref() {
            quotes.get(index).and_then(|q| q.value_usd).unwrap_or(0.0)
        } else {
            pricing.quote(record).value_usd.unwrap_or(0.0)
        };

        let add = |b: &mut TemporalUsageBucket| {
            b.tokens = b.tokens.saturating_add(tok);
            b.input = b.input.saturating_add(inp);
            b.output = b.output.saturating_add(out);
            b.cache = b.cache.saturating_add(cch);
            b.api_usd += val;
            b.records += 1;
        };

        add(&mut all);
        if record.date == today_date || (today_start_ms > 0 && record.timestamp >= today_start_ms) {
            add(&mut today);
        }
        if record.timestamp >= h24_ms {
            add(&mut h24);
        }
        if record.timestamp >= d7_ms {
            add(&mut d7);
        }
        if record.timestamp >= d30_ms {
            add(&mut d30);
        }
    }

    vec![today, h24, d7, d30, all]
}


#[derive(Clone, Default)]
struct ModelConsumptionRow {
    model: String,
    records: usize,
    tokens: usage::UsageTokens,
    api_value_usd: f64,
    clients: std::collections::HashMap<String, usize>,
}

impl ModelConsumptionRow {
    fn token_total(&self) -> i64 {
        self.tokens.reported_total_without_reasoning()
    }

    fn primary_client(&self) -> String {
        let mut sorted: Vec<(&String, &usize)> = self.clients.iter().collect();
        sorted.sort_by_key(|a| std::cmp::Reverse(*a.1));
        sorted.into_iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>().join(", ")
    }
}

fn model_consumption_rows(
    report: &usage::ConsumptionReport,
    metric: ConsumptionMetric,
    window: LedgerWindow,
    query: &str,
) -> Vec<ModelConsumptionRow> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let today_str = local_today_str();
    let mut rows: std::collections::HashMap<String, ModelConsumptionRow> = std::collections::HashMap::new();
    let pricing = token_monitor_core::pricing::PricingEngine::load_cached();

    let q = query.trim().to_ascii_lowercase();
    for (index, record) in report.snapshot.records.iter().enumerate() {
        if !record_in_window(record, window, now_ms, &today_str) {
            continue;
        }
        let client_norm = normalize_client_id(&record.client);
        if !q.is_empty() {
            let m1 = record.model_id.to_ascii_lowercase().contains(&q);
            let m2 = client_norm.to_ascii_lowercase().contains(&q);
            if !m1 && !m2 {
                continue;
            }
        }
        let entry = rows.entry(record.model_id.clone()).or_insert_with(|| ModelConsumptionRow {
            model: record.model_id.clone(),
            records: 0,
            tokens: usage::UsageTokens::default(),
            api_value_usd: 0.0,
            clients: std::collections::HashMap::new(),
        });
        entry.records += 1;
        entry.tokens.add_assign(&record.tokens);
        *entry.clients.entry(client_norm).or_insert(0) += 1;
        let val = if let Some(quotes) = report.quotes.as_ref() {
            quotes.get(index).and_then(|q| q.value_usd).unwrap_or_else(|| pricing.quote(record).value_usd.unwrap_or(0.0))
        } else {
            pricing.quote(record).value_usd.unwrap_or(0.0)
        };
        entry.api_value_usd += val;
    }

    let mut list: Vec<ModelConsumptionRow> = rows.into_values().collect();
    list.sort_by(|left, right| {
        match metric {
            ConsumptionMetric::Tokens => right.token_total().cmp(&left.token_total()),
            ConsumptionMetric::ApiEquivalent => right
                .api_value_usd
                .partial_cmp(&left.api_value_usd)
                .unwrap_or(std::cmp::Ordering::Equal),
        }
    });
    list
}

fn consumption_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let Some(report) = &app.consumption else {
        return vec![Line::from(Span::styled(
            "Loading local consumption ledger...",
            Style::default().fg(GREY),
        ))];
    };

    let (buckets, rows, m_rows) = if let Some(cached) = &app.cached_ledger {
        let win_idx = match app.ledger_window {
            LedgerWindow::Today => 0,
            LedgerWindow::Last24h => 1,
            LedgerWindow::Last7d => 2,
            LedgerWindow::Last30d => 3,
            LedgerWindow::AllTime => 4,
        };
        let mut r = cached.clients[win_idx].clone();
        r.sort_by(|left, right| match app.consumption_metric {
            ConsumptionMetric::Tokens => right.token_total().cmp(&left.token_total()),
            ConsumptionMetric::ApiEquivalent => right
                .api_value_usd
                .partial_cmp(&left.api_value_usd)
                .unwrap_or(std::cmp::Ordering::Equal),
        });
        if !app.search_query.is_empty() {
            let q = app.search_query.trim().to_ascii_lowercase();
            r.retain(|row| {
                row.client.to_ascii_lowercase().contains(&q)
                    || row.provider.to_ascii_lowercase().contains(&q)
            });
        }

        let mut mr = cached.models[win_idx].clone();
        mr.sort_by(|left, right| match app.consumption_metric {
            ConsumptionMetric::Tokens => right.token_total().cmp(&left.token_total()),
            ConsumptionMetric::ApiEquivalent => right
                .api_value_usd
                .partial_cmp(&left.api_value_usd)
                .unwrap_or(std::cmp::Ordering::Equal),
        });
        if !app.search_query.is_empty() {
            let q = app.search_query.trim().to_ascii_lowercase();
            mr.retain(|row| row.model.to_ascii_lowercase().contains(&q));
        }
        (cached.buckets.clone(), r, mr)
    } else {
        let b = compute_temporal_usage(report);
        let r = consumption_rows(report, app.consumption_metric, app.ledger_window, &app.search_query);
        let mr = model_consumption_rows(report, app.consumption_metric, app.ledger_window, &app.search_query);
        (b, r, mr)
    };

    let mut lines = Vec::new();
    let daily_bucket = buckets.iter().find(|b| b.label == "Last 24h").or_else(|| buckets.first());
    let daily_tok = daily_bucket.map(|b| b.tokens).unwrap_or(0);
    let daily_usd = daily_bucket.map(|b| b.api_usd).unwrap_or(0.0);
    let monthly_tok = daily_tok.saturating_mul(30);
    let monthly_usd = daily_usd * 30.0;
    let eff_24h = if daily_usd > 0.0 {
        format!("{:.2}M/$", daily_tok as f64 / 1_000_000.0 / daily_usd)
    } else {
        "—".to_owned()
    };

    lines.push(Line::from(""));
    if width >= 115 {
        lines.push(Line::from(vec![
            Span::styled("BURN VELOCITY (24h): ", Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::styled(format!("≈ {}/day", format_tokens(daily_tok)), Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" (${:.2}/day)", daily_usd), Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
            Span::raw("  ·  "),
            Span::styled("PROJECTED: ", Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::styled(format!("≈ {}/mo", format_tokens(monthly_tok)), Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" (${:.0}/mo)", monthly_usd), Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
            Span::raw("  ·  "),
            Span::styled("EFFICIENCY: ", Style::default().fg(GREY)),
            Span::styled(eff_24h, Style::default().fg(Color::Rgb(66, 165, 245)).add_modifier(Modifier::BOLD)),
            Span::raw("  ·  actual "),
            Span::styled("—", Style::default().fg(GREY)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("24h Burn: ", Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::styled(format!("≈ {}/d", format_tokens(daily_tok)), Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" (${:.2}/d)", daily_usd), Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
            Span::raw("  ·  "),
            Span::styled("Proj: ", Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::styled(format!("≈ {}/mo", format_tokens(monthly_tok)), Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" (${:.0}/mo)", monthly_usd), Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
            Span::raw("  ·  "),
            Span::styled(eff_24h, Style::default().fg(Color::Rgb(66, 165, 245))),
            Span::raw("  ·  actual "),
            Span::styled("—", Style::default().fg(GREY)),
        ]));
    }

    lines.push(Line::from(""));
    let banner_time = if width >= 115 {
        "── TOKEN USAGE BY TIME WINDOW ── [h/l or ←/→: switch window · Enter: detail] "
    } else {
        "── TOKEN USAGE BY TIME WINDOW ── [h/l: win · Enter: detail] "
    };
    let banner_time_pad = (width as usize).saturating_sub(str_width(banner_time));
    lines.push(Line::from(vec![
        Span::styled(banner_time, Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(banner_time_pad), Style::default().fg(DIM_GREY)),
    ]));

    let col_w = if width >= 115 { 13 } else { 10 };
    lines.push(Line::from(vec![
        Span::styled(fit("WINDOW", 12), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(fit("TOTAL TOKENS", col_w), Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(fit("INPUT", col_w), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(fit("OUTPUT", col_w), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(fit("CACHE", col_w), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(fit("API-EQ", 10), Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(fit("TOK/$ EFF", 10), Style::default().fg(Color::Rgb(66, 165, 245)).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(fit("RECORDS", 8), Style::default().fg(DIM_GREY).add_modifier(Modifier::BOLD)),
    ]));

    lines.push(Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(DIM_GREY),
    )));

    for (win_idx, bucket) in buckets.iter().enumerate() {
        let cost_str = if bucket.api_usd > 0.0 {
            format!("${:.2}", bucket.api_usd)
        } else {
            "—".to_owned()
        };
        let eff_str = if bucket.api_usd > 0.0 {
            format!("{:.2}M/$", bucket.tokens as f64 / 1_000_000.0 / bucket.api_usd)
        } else {
            "—".to_owned()
        };
        let (window_color, bold) = match bucket.label {
            "Today" => (Color::Rgb(66, 165, 245), true),
            "Last 24h" => (CYAN, true),
            "Last 7d" => (YELLOW, false),
            "Last 30d" => (Color::Rgb(187, 154, 247), false),
            _ => (Color::White, false),
        };

        let is_active_win = matches!(
            (win_idx, app.ledger_window),
            (0, LedgerWindow::Today)
                | (1, LedgerWindow::Last24h)
                | (2, LedgerWindow::Last7d)
                | (3, LedgerWindow::Last30d)
                | (4, LedgerWindow::AllTime)
        );
        let is_sel = app.consumption_selected == 0 && is_active_win;

        let cursor_mark = if is_sel { "▶ " } else { "  " };
        let active_mark = if is_active_win { "● " } else { "  " };
        let label_text = format!("{}{}", active_mark, bucket.label);
        let mut label_style = Style::default().fg(if is_active_win { Color::White } else { window_color }).add_modifier(if is_active_win || bold { Modifier::BOLD } else { Modifier::empty() });
        if is_sel {
            label_style = label_style.bg(Color::Rgb(28, 42, 60));
        }

        lines.push(Line::from(vec![
            Span::styled(cursor_mark, Style::default().fg(if is_sel { Color::Yellow } else { Color::Reset }).add_modifier(Modifier::BOLD)),
            Span::styled(
                fit(&label_text, 12),
                label_style,
            ),
            Span::raw("  "),
            Span::styled(
                fit(&format_tokens(bucket.tokens), col_w),
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(fit(&format_tokens(bucket.input), col_w), Style::default().fg(GREY)),
            Span::raw("  "),
            Span::styled(fit(&format_tokens(bucket.output), col_w), Style::default().fg(GREY)),
            Span::raw("  "),
            Span::styled(fit(&format_tokens(bucket.cache), col_w), Style::default().fg(GREY)),
            Span::raw("  "),
            Span::styled(
                fit(&cost_str, 10),
                Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                fit(&eff_str, 10),
                Style::default().fg(Color::Rgb(66, 165, 245)),
            ),
            Span::raw("  "),
            Span::styled(fit(&bucket.records.to_string(), 8), Style::default().fg(DIM_GREY)),
        ]));
    }

    lines.push(Line::from(""));
    let banner_rows = format!("── PER-CLIENT & SUBSCRIPTION BREAKDOWN [WINDOW: {}] ── [w] Switch Window ", app.ledger_window.label());
    let banner_rows_pad = (width as usize).saturating_sub(str_width(&banner_rows));
    lines.push(Line::from(vec![
        Span::styled(banner_rows, Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(banner_rows_pad), Style::default().fg(DIM_GREY)),
    ]));

    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("No local usage records in {} window.", app.ledger_window.label()),
            Style::default().fg(GREY),
        )));
    } else {
        let label_width = if width >= 120 { 26 } else { 20 };
        let token_width = 12;
        let api_width = 12;
        let coverage_width = 12;
        let bar_width = (width as usize)
            .saturating_sub(label_width + token_width + api_width + coverage_width + 8)
            .clamp(6, 18);
        lines.push(Line::from(vec![
            Span::styled(fit("SOURCE", label_width), Style::default().fg(GREY)),
            Span::raw("  "),
            Span::styled(fit("TOKENS", token_width), Style::default().fg(GREY)),
            Span::raw("  "),
            Span::styled(fit("API-EQ", api_width), Style::default().fg(GREY)),
            Span::raw("  "),
            Span::styled(fit("PRICED", coverage_width), Style::default().fg(GREY)),
            Span::raw("  "),
            Span::styled("USAGE", Style::default().fg(GREY)),
        ]));
        lines.push(Line::from(Span::styled(
            "─".repeat(width as usize),
            Style::default().fg(GREY),
        )));
        let max_tokens = rows
            .iter()
            .map(ConsumptionRow::token_total)
            .max()
            .unwrap_or(0);
        for (row_idx, row) in rows.iter().enumerate() {
            let is_sel = app.consumption_selected == 1 + row_idx;
            let priced = row
                .coverage()
                .map(|value| format!("{:>5.1}%", value * 100.0))
                .unwrap_or_else(|| "    —".into());
            let token_text = format_tokens(row.token_total());
            let brand_color = client_brand_color(&row.client);
            let usage_bar = colored_meter(
                if max_tokens > 0 {
                    row.token_total() as f64 / max_tokens as f64 * 100.0
                } else {
                    0.0
                },
                bar_width,
                brand_color,
            );
            let mut label_style = Style::default().fg(brand_color);
            if is_sel {
                label_style = label_style.bg(Color::Rgb(28, 42, 60)).add_modifier(Modifier::BOLD);
            }
            let cursor_mark = if is_sel { "▶ " } else { "  " };
            let mut row_spans = vec![
                Span::styled(cursor_mark, Style::default().fg(if is_sel { Color::Yellow } else { Color::Reset }).add_modifier(Modifier::BOLD)),
                Span::styled(fit(&row.client, label_width.saturating_sub(2)), label_style),
                Span::raw("  "),
                Span::styled(
                    fit(&token_text, token_width),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw("  "),
                Span::styled(fit(&row.api_text(), api_width), Style::default().fg(YELLOW)),
                Span::raw("  "),
                Span::styled(fit(&priced, coverage_width), Style::default().fg(GREY)),
                Span::raw("  "),
            ];
            row_spans.extend(usage_bar);
            lines.push(Line::from(row_spans));
        }
    }

    // Section: Top Model Mix
    lines.push(Line::from(""));
    let banner_models = format!("── TOP MODEL MIX [WINDOW: {}] ── ", app.ledger_window.label());
    let banner_models_pad = (width as usize).saturating_sub(str_width(&banner_models));
    lines.push(Line::from(vec![
        Span::styled(banner_models, Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(banner_models_pad), Style::default().fg(DIM_GREY)),
    ]));

    let max_m_tokens = m_rows.iter().map(|m| m.token_total()).max().unwrap_or(0);
    let m_label_w = if width >= 120 { 28 } else { 22 };
    let harness_w = if width >= 120 { 18 } else { 14 };
    let token_width = 12;
    let api_width = 10;
    let coverage_width = 11;
    let m_bar_width = (width as usize)
        .saturating_sub(m_label_w + harness_w + token_width + api_width + coverage_width + 10)
        .clamp(4, 18);

    lines.push(Line::from(vec![
        Span::styled(fit("MODEL", m_label_w), Style::default().fg(GREY)),
        Span::raw("  "),
        Span::styled(fit("HARNESS", harness_w), Style::default().fg(GREY)),
        Span::raw("  "),
        Span::styled(fit("TOKENS", token_width), Style::default().fg(GREY)),
        Span::raw("  "),
        Span::styled(fit("API-EQ", api_width), Style::default().fg(GREY)),
        Span::raw("  "),
        Span::styled(fit("TOK/$ EFF", coverage_width), Style::default().fg(GREY)),
        Span::raw("  "),
        Span::styled("USAGE", Style::default().fg(GREY)),
    ]));
    lines.push(Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(DIM_GREY),
    )));

    let num_clients = rows.len();
    for (m_idx, m_row) in m_rows.into_iter().take(10).enumerate() {
        let is_sel = app.consumption_selected == 1 + num_clients + m_idx;
        let m_tokens = m_row.token_total();
        let token_text = format_tokens(m_tokens);
        let api_text = if m_row.api_value_usd > 0.0 {
            format!("${:.2}", m_row.api_value_usd)
        } else {
            "—".to_owned()
        };
        let eff_str = if m_row.api_value_usd > 0.0 {
            format!("{:.2}M/$", m_tokens as f64 / 1_000_000.0 / m_row.api_value_usd)
        } else {
            "—".to_owned()
        };

        let hue = m_row.model.bytes().fold(0u8, |sum, b| sum.wrapping_add(b));
        let m_color = palette(hue);
        let mut m_style = Style::default().fg(m_color);
        if is_sel {
            m_style = m_style.bg(Color::Rgb(28, 42, 60)).add_modifier(Modifier::BOLD);
        }
        let cursor_mark = if is_sel { "▶ " } else { "  " };
        let parent_harness = m_row.primary_client();
        let h_brand = client_brand_color(&parent_harness);

        let m_meter = colored_meter(
            if max_m_tokens > 0 {
                m_tokens as f64 / max_m_tokens as f64 * 100.0
            } else {
                0.0
            },
            m_bar_width,
            m_color,
        );

        let mut row_spans = vec![
            Span::styled(cursor_mark, Style::default().fg(if is_sel { Color::Yellow } else { Color::Reset }).add_modifier(Modifier::BOLD)),
            Span::styled(fit(&m_row.model, m_label_w.saturating_sub(2)), m_style),
            Span::raw("  "),
            Span::styled(fit(&parent_harness, harness_w), Style::default().fg(h_brand)),
            Span::raw("  "),
            Span::styled(fit(&token_text, token_width), Style::default().fg(CYAN)),
            Span::raw("  "),
            Span::styled(fit(&api_text, api_width), Style::default().fg(YELLOW)),
            Span::raw("  "),
            Span::styled(fit(&eff_str, coverage_width), Style::default().fg(Color::Rgb(66, 165, 245))),
            Span::raw("  "),
        ];
        row_spans.extend(m_meter);
        lines.push(Line::from(row_spans));
    }

    lines
}

#[allow(dead_code)]
fn window_value_style(provider: &ProviderSnapshot, window: Option<&LimitWindow>) -> Style {
    let color = window
        .and_then(|window| window.remaining_percent)
        .map(|value| {
            if value <= 10.0 {
                RED
            } else if value <= 25.0 {
                YELLOW
            } else {
                palette(provider.hue)
            }
        })
        .unwrap_or_else(|| palette(provider.hue));
    let mut style = Style::default().fg(color);
    if row_dimmed(provider) {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

#[allow(dead_code)]
fn detailed_window_lines(
    provider: &ProviderSnapshot,
    width: usize,
    label_width: usize,
    reset_enabled: bool,
    credit: bool,
) -> Vec<(String, Option<String>)> {
    let windows = ordered_windows(provider)
        .into_iter()
        .filter(|window| window.metric.is_credit() == credit)
        .collect::<Vec<_>>();
    if reset_enabled {
        windows
            .into_iter()
            .map(|window| {
                let text = if credit {
                    credit_summary(window, width)
                } else {
                    quota_summary(window, width, label_width)
                };
                (text, Some(reset_text(window)))
            })
            .collect()
    } else {
        let texts = windows
            .into_iter()
            .map(|window| {
                if credit {
                    credit_summary(window, width)
                } else {
                    quota_summary(window, width, label_width)
                }
            })
            .collect::<Vec<_>>();
        pack_texts(texts, width)
            .into_iter()
            .map(|text| (text, None))
            .collect()
    }
}

#[allow(dead_code)]
fn indent_lines(lines: Vec<Line<'static>>, indent: usize) -> Vec<Line<'static>> {
    if indent == 0 {
        return lines;
    }
    lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

fn settings_status_style(status: &str) -> Style {
    let color = match status {
        "connected" | "saved · refresh" => GREEN,
        "unauthorized" | "error" | "exhausted" | "blocked" => RED,
        "stale" => YELLOW,
        _ => GREY,
    };
    Style::default().fg(color)
}

fn settings_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let state = &app.settings;
    let provider_id = state
        .provider_ids
        .get(state.selected)
        .map(String::as_str)
        .unwrap_or("unknown");
    let provider_label = token_monitor_core::provider_registry::display_name(provider_id);
    if state.editing {
        let Some(spec) = credential_spec(provider_id) else {
            return vec![Line::from("This provider is auto-discovered; press Esc.")];
        };
        let masked = if state.input.is_empty() {
            "(empty)".into()
        } else {
            "•".repeat(state.input.chars().count())
        };
        let mut lines = vec![
            Line::from(Span::styled(
                format!("SETTINGS · configure {provider_label}"),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(format!("credential type · {}", spec.kind.label())),
            Line::from(format!("provider page · https://{}", spec.url)),
            Line::from("flow · open page → copy credential → paste here → Enter"),
            Line::from(Span::styled(spec.instruction, Style::default().fg(GREY))),
        ];
        if !spec.prefix.is_empty() {
            lines.push(Line::from(format!("expected prefix · {}", spec.prefix)));
        }
        lines.extend([
            Line::from(""),
            Line::from(vec![
                Span::styled("secret · ", Style::default().fg(GREY)),
                Span::styled(masked, Style::default().fg(Color::White)),
            ]),
            Line::from(format!(
                "store · {}",
                credentials::path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "unavailable".into())
            )),
            Line::from("Enter save · Esc cancel · paste is accepted directly"),
        ]);
        if !state.message.is_empty() {
            lines.push(Line::from(Span::styled(
                state.message.clone(),
                Style::default().fg(YELLOW),
            )));
        }
        return lines;
    }

    let mut lines = vec![
        Line::from(Span::styled(
            "SETTINGS · credentials",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Enter edit · d remove · ↑/↓ choose · Esc back · credentials stay local",
            Style::default().fg(GREY),
        )),
        Line::from(""),
    ];
    let provider_width = if width >= 100 { 28 } else { 22 };
    let auth_width = if width >= 100 { 20 } else { 16 };
    lines.push(Line::from(vec![
        Span::styled(fit("PROVIDER", provider_width), Style::default().fg(GREY)),
        Span::raw("  "),
        Span::styled(fit("AUTH", auth_width), Style::default().fg(GREY)),
        Span::raw("  STATUS"),
    ]));
    lines.push(Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(GREY),
    )));
    for (index, id) in state.provider_ids.iter().enumerate() {
        let selected = index == state.selected;
        let label = token_monitor_core::provider_registry::display_name(id);
        let auth = credential_spec(id)
            .map(|spec| spec.kind.label())
            .unwrap_or("auto-discover");
        let status = app.settings_status(id);
        let hue = id.bytes().fold(0u8, |sum, value| sum.wrapping_add(value));
        let mut provider_style = Style::default().fg(palette(hue));
        if selected {
            provider_style = provider_style.add_modifier(Modifier::BOLD);
        }
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "▶ " } else { "  " },
                Style::default().fg(Color::White),
            ),
            Span::styled(fit(label, provider_width.saturating_sub(2)), provider_style),
            Span::raw("  "),
            Span::styled(fit(auth, auth_width), Style::default().fg(GREY)),
            Span::raw("  "),
            Span::styled(status.clone(), settings_status_style(&status)),
        ]));
    }
    if !state.message.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            state.message.clone(),
            Style::default().fg(YELLOW),
        )));
    }
    lines
}


fn provider_brand_color(provider_id: &str) -> Color {
    match provider_id.to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Color::Rgb(217, 119, 87),    // Anthropic Terracotta #D97757
        "codex" | "openai" => Color::Rgb(16, 163, 127),        // OpenAI Jade #10A37F
        "antigravity" | "google" => Color::Rgb(66, 133, 244),   // Google Azure #4285F4
        "cursor" => Color::Rgb(0, 180, 216),                   // Cursor Cyan #00B4D8
        "commandcode" => Color::Rgb(245, 175, 45),              // Command Code Gold #F5AF2D
        "grok" | "xai" => Color::Rgb(210, 215, 225),            // xAI Silver #D2D7E1
        "modal" => Color::Rgb(0, 210, 106),                     // Modal Emerald #00D26A
        "openrouter" => Color::Rgb(140, 100, 255),              // OpenRouter Royal Violet #7624F4
        "vast" | "vastai" => Color::Rgb(255, 140, 50),          // Vast.ai Sunset Orange #FF8C32
        "deepseek" => Color::Rgb(77, 107, 254),                 // DeepSeek Royal Blue #4D6BFE
        "copilot" | "github" => Color::Rgb(46, 160, 67),        // GitHub Copilot Mint #2EA043
        "kimi" => Color::Rgb(255, 110, 180),                    // Kimi Pink #FF6EB4
        "zai" | "zaiteam" => Color::Rgb(168, 85, 247),          // Z.ai Violet #A855F7
        "kiro" => Color::Rgb(34, 211, 238),                     // Kiro Cyan #22D3EE
        "minimax" => Color::Rgb(251, 146, 60),                  // MiniMax Orange #FB923C
        "ollama" => Color::Rgb(34, 197, 94),                    // Ollama Green #22C55E
        _ => Color::Rgb(125, 207, 255),                         // Default Cyan
    }
}

fn is_wallet_provider(provider: &ProviderSnapshot) -> bool {
    let id = provider.provider_id.to_ascii_lowercase();
    if id == "modal" || id == "openrouter" || id == "vast" || id == "vastai" || id == "deepseek" {
        return true;
    }
    if provider.payg() {
        return true;
    }
    let has_quota = provider.windows.iter().any(|w| w.metric == WindowMetric::Quota);
    let has_credit = provider.windows.iter().any(|w| w.metric.is_credit());
    !has_quota && has_credit
}

struct MatrixColumns {
    provider: usize,
    session: usize,
    cycle: usize,
    reset: usize,
    status: usize,
    bar_width: usize,
}

fn calculate_matrix_columns(width: u16) -> MatrixColumns {
    let w = width as usize;
    if w >= 150 {
        MatrixColumns {
            provider: 38,
            session: 22,
            cycle: 22,
            reset: 26,
            status: w.saturating_sub(38 + 22 + 22 + 26 + 8).max(18),
            bar_width: 12,
        }
    } else if w >= 120 {
        MatrixColumns {
            provider: 32,
            session: 18,
            cycle: 18,
            reset: 24,
            status: w.saturating_sub(32 + 18 + 18 + 24 + 8).max(14),
            bar_width: 8,
        }
    } else if w >= 95 {
        MatrixColumns {
            provider: 26,
            session: 15,
            cycle: 15,
            reset: 20,
            status: w.saturating_sub(26 + 15 + 15 + 20 + 8).max(12),
            bar_width: 6,
        }
    } else if w >= 75 {
        MatrixColumns {
            provider: 20,
            session: 13,
            cycle: 13,
            reset: 15,
            status: w.saturating_sub(20 + 13 + 13 + 15 + 8).max(10),
            bar_width: 4,
        }
    } else {
        MatrixColumns {
            provider: 16,
            session: 12,
            cycle: 12,
            reset: 12,
            status: w.saturating_sub(16 + 12 + 12 + 12 + 8).max(6),
            bar_width: 4,
        }
    }
}

fn format_quota_cell(
    window: Option<&LimitWindow>,
    bar_width: usize,
    col_width: usize,
    brand_color: Color,
    dimmed: bool,
) -> Vec<Span<'static>> {
    if let Some(w) = window {
        if let Some(pct) = w.remaining_percent {
            let color = if pct <= 0.0 {
                RED
            } else if pct < 20.0 {
                YELLOW
            } else {
                brand_color
            };
            let m = meter(pct, bar_width);
            let pct_text = if col_width < 18 {
                format!("{:.0}%", pct)
            } else {
                format!("{:>5.1}%", pct)
            };
            let mut style = Style::default().fg(color);
            if dimmed || pct <= 0.0 {
                style = style.add_modifier(Modifier::DIM);
            }
            if pct <= 0.0 {
                style = style.add_modifier(Modifier::BOLD);
            }
            let mut spans = vec![
                Span::styled(format!("[{m}]"), style),
                Span::raw(" "),
                Span::styled(pct_text, style),
            ];
            let current_w: usize = spans.iter().map(|s| str_width(&s.content)).sum();
            if current_w < col_width {
                spans.push(Span::raw(" ".repeat(col_width - current_w)));
            }
            spans
        } else if let Some(amt) = w.remaining_amount {
            let curr = w.currency.as_deref().unwrap_or("$");
            let text = format!("{}{:.2}", curr, amt);
            let mut style = Style::default().fg(brand_color);
            if dimmed {
                style = style.add_modifier(Modifier::DIM);
            }
            vec![Span::styled(fit(&text, col_width), style)]
        } else {
            vec![Span::styled(fit("—", col_width), Style::default().fg(DIM_GREY))]
        }
    } else {
        vec![Span::styled(fit("—", col_width), Style::default().fg(DIM_GREY))]
    }
}


struct BurnRecommendation {
    provider_title: String,
    remaining_pct: f64,
    resets_in: String,
    reset_ms: i64,
}

fn find_burn_first_recommendation(providers: &[&ProviderSnapshot], now_ms: i64) -> Option<BurnRecommendation> {
    let mut candidates: Vec<BurnRecommendation> = Vec::new();
    for p in providers {
        if is_wallet_provider(p) {
            continue;
        }
        for w in &p.windows {
            if (w.kind == WindowKind::Weekly || w.kind == WindowKind::Monthly || w.label.to_ascii_lowercase().contains("7d"))
                && !w.metric.is_credit()
            {
                if let (Some(pct), Some(reset_ms)) = (w.remaining_percent, w.resets_at_ms) {
                    if pct > 10.0 && reset_ms > now_ms {
                        let diff_ms = reset_ms - now_ms;
                        if diff_ms < 5 * 86400 * 1000 {
                            candidates.push(BurnRecommendation {
                                provider_title: provider_title_base(p),
                                remaining_pct: pct,
                                resets_in: reset_text(w),
                                reset_ms,
                            });
                        }
                    }
                }
            }
        }
    }
    candidates.sort_by_key(|c| c.reset_ms);
    candidates.into_iter().next()
}


fn format_dual_reset(
    session_window: Option<&LimitWindow>,
    cycle_window: Option<&LimitWindow>,
    col_width: usize,
    now_ms: i64,
    dimmed: bool,
) -> Vec<Span<'static>> {
    let s5_raw = session_window.map(reset_text).unwrap_or_default();
    let s7_raw = cycle_window.map(reset_text).unwrap_or_default();

    let s5 = if !s5_raw.is_empty() && s5_raw != "—" {
        format!("↻ {s5_raw}")
    } else {
        "—".to_owned()
    };
    let s7 = if !s7_raw.is_empty() && s7_raw != "—" {
        format!("↻ {s7_raw}")
    } else {
        "—".to_owned()
    };

    let is_urgent = cycle_window.is_some_and(|w| {
        if let (Some(r), Some(p)) = (w.resets_at_ms, w.remaining_percent) {
            let diff = r - now_ms;
            diff > 0 && diff <= 2 * 86400 * 1000 && p > 10.0
        } else {
            false
        }
    });

    let s5_color = if s5.contains('m') || s5.contains("0m") {
        YELLOW
    } else {
        GREY
    };

    let s7_color = if is_urgent {
        Color::Rgb(245, 175, 45) // Amber gold: expiring soonest!
    } else if s7.contains('d') {
        GREY
    } else {
        YELLOW
    };

    let s5_compact = if col_width <= 18 && s5.len() > 6 && s5.contains('h') && s5.contains('m') {
        let parts: Vec<&str> = s5.split_whitespace().collect();
        if parts.len() >= 2 {
            format!("{} {}", parts[0], parts[1])
        } else {
            s5.clone()
        }
    } else {
        s5.clone()
    };

    let (s5_w, s7_w) = if col_width >= 24 {
        (10, col_width.saturating_sub(13))
    } else if col_width >= 20 {
        (8, col_width.saturating_sub(11))
    } else if col_width >= 16 {
        (6, col_width.saturating_sub(9))
    } else {
        (5, col_width.saturating_sub(8))
    };

    let mut s5_style = Style::default().fg(s5_color);
    let mut s7_style = Style::default().fg(s7_color);
    if is_urgent {
        s7_style = s7_style.add_modifier(Modifier::BOLD);
    }
    if dimmed {
        s5_style = s5_style.add_modifier(Modifier::DIM);
        s7_style = s7_style.add_modifier(Modifier::DIM);
    }

    vec![
        Span::styled(fit(&s5_compact, s5_w), s5_style),
        Span::styled(" │ ", Style::default().fg(DIM_GREY)),
        Span::styled(fit(&s7, s7_w), s7_style),
    ]
}

#[allow(dead_code)]
fn format_cycle_reset(window: Option<&LimitWindow>, col_width: usize, now_ms: i64, dimmed: bool) -> Span<'static> {
    if let Some(w) = window {
        let text = reset_text(w);
        if text.trim().is_empty() || text == "—" {
            Span::styled(fit("—", col_width), Style::default().fg(DIM_GREY))
        } else {
            let formatted = format!("↻ {text}");
            let is_urgent = w.resets_at_ms.is_some_and(|r| {
                let diff = r - now_ms;
                diff > 0 && diff <= 2 * 86400 * 1000 && w.remaining_percent.is_some_and(|p| p > 10.0)
            });
            let color = if is_urgent {
                Color::Rgb(245, 175, 45) // Amber gold: expiring soon!
            } else if text.contains('d') {
                GREY
            } else {
                YELLOW
            };
            let mut style = Style::default().fg(color);
            if is_urgent {
                style = style.add_modifier(Modifier::BOLD);
            }
            if dimmed {
                style = style.add_modifier(Modifier::DIM);
            }
            Span::styled(fit(&formatted, col_width), style)
        }
    } else {
        Span::styled(fit("—", col_width), Style::default().fg(DIM_GREY))
    }
}

#[allow(dead_code)]
fn format_matrix_reset(window: Option<&LimitWindow>, col_width: usize) -> Span<'static> {
    if let Some(w) = window {
        let text = reset_text(w);
        if text.trim().is_empty() || text == "—" {
            Span::styled(fit("—", col_width), Style::default().fg(DIM_GREY))
        } else {
            let formatted = format!("↻ {text}");
            let color = if text.contains('m') || text.contains("0m") {
                YELLOW
            } else if text.contains('d') {
                GREY
            } else {
                YELLOW
            };
            Span::styled(fit(&formatted, col_width), Style::default().fg(color))
        }
    } else {
        Span::styled(fit("—", col_width), Style::default().fg(DIM_GREY))
    }
}

fn format_matrix_status(
    provider: &ProviderSnapshot,
    override_text: Option<(&str, Color)>,
    col_width: usize,
    is_burn_first: bool,
) -> Span<'static> {
    if is_burn_first && provider.availability == Availability::Available {
        return Span::styled(
            fit("🔥 BURN FIRST", col_width),
            Style::default().fg(Color::Rgb(245, 175, 45)).add_modifier(Modifier::BOLD),
        );
    }
    if let Some((text, color)) = override_text {
        return Span::styled(
            fit(text, col_width),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        );
    }
    let (text, color) = match provider.availability {
        Availability::Exhausted => {
            let has_reset_credit = provider
                .diagnostics
                .iter()
                .any(|d| d.to_ascii_lowercase().contains("reset credit") || d.contains("★"));
            if has_reset_credit {
                ("exhausted (★1 credit)", RED)
            } else {
                ("exhausted", RED)
            }
        }
        Availability::AgentBlocked => ("blocked", RED),
        Availability::Available => ("ready", GREEN),
        Availability::Unknown => ("unknown", GREY),
    };
    let mut style = Style::default().fg(color);
    if provider.availability.dimmed() {
        style = style.add_modifier(Modifier::DIM);
    } else {
        style = style.add_modifier(Modifier::BOLD);
    }
    Span::styled(fit(text, col_width), style)
}

fn limits_matrix_lines(app: &App, width: u16, height: u16) -> Vec<Line<'static>> {
    let visible = app.visible_providers();
    if visible.is_empty() {
        return vec![Line::from(Span::styled(
            "No providers match this filter.",
            Style::default().fg(GREY),
        ))];
    }

    let mut subscriptions = Vec::new();
    let mut wallets = Vec::new();

    for provider in visible {
        if is_wallet_provider(provider) {
            wallets.push(provider);
        } else {
            subscriptions.push(provider);
        }
    }

    let cols = calculate_matrix_columns(width);
    let mut lines = Vec::new();

    // Section 1: AI Subscriptions
    if !subscriptions.is_empty() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let burn_rec = find_burn_first_recommendation(&subscriptions, now_ms);
        let banner_text = if let Some(rec) = &burn_rec {
            if width >= 115 {
                format!("── AI SUBSCRIPTIONS & ROLLING RATE-LIMITS ── 🔥 Burn First: {} ({}% left, resets in {}) ", rec.provider_title, rec.remaining_pct as u32, rec.resets_in)
            } else {
                format!("── AI SUBSCRIPTIONS ── 🔥 Burn First: {} ({}%, ↻ {}) ", rec.provider_title, rec.remaining_pct as u32, rec.resets_in)
            }
        } else {
            if width >= 115 {
                "── AI SUBSCRIPTIONS & ROLLING RATE-LIMITS ".to_owned()
            } else {
                "── AI SUBSCRIPTIONS ".to_owned()
            }
        };
        let b_len = str_width(&banner_text);
        let max_w = width as usize;
        if b_len >= max_w {
            lines.push(Line::from(Span::styled(
                fit(&banner_text, max_w),
                Style::default().fg(if burn_rec.is_some() { Color::Rgb(245, 175, 45) } else { GREY }).add_modifier(Modifier::BOLD),
            )));
        } else {
            let banner_pad = max_w - b_len;
            lines.push(Line::from(vec![
                Span::styled(banner_text, Style::default().fg(if burn_rec.is_some() { Color::Rgb(245, 175, 45) } else { GREY }).add_modifier(Modifier::BOLD)),
                Span::styled("─".repeat(banner_pad), Style::default().fg(DIM_GREY)),
            ]));
        }

        lines.push(Line::from(vec![
            Span::styled(fit("SUBSCRIPTION / POOL", cols.provider), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(fit("SESSION (5H)", cols.session), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(fit("CYCLE (7D)", cols.cycle), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(fit("RESETS (5H │ 7D)", cols.reset), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(fit("STATUS / BURN", cols.status), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
        ]));

        lines.push(Line::from(Span::styled(
            "─".repeat(width as usize),
            Style::default().fg(DIM_GREY),
        )));

        for (s_idx, provider) in subscriptions.iter().enumerate() {
            let pid = provider.provider_id.to_ascii_lowercase();

            // Antigravity Special Multi-Pool Rendering
            if pid == "antigravity" {
                let gemini_5h = provider.windows.iter().find(|w| {
                    let l = w.label.to_ascii_lowercase();
                    l.contains("gemini") && (l.contains("5h") || w.kind == WindowKind::Session)
                });
                let gemini_7d = provider.windows.iter().find(|w| {
                    let l = w.label.to_ascii_lowercase();
                    l.contains("gemini") && (l.contains("7d") || w.kind == WindowKind::Weekly)
                });
                let claude_5h = provider.windows.iter().find(|w| {
                    let l = w.label.to_ascii_lowercase();
                    (l.contains("claude") || l.contains("gpt")) && (l.contains("5h") || w.kind == WindowKind::Session)
                });
                let claude_7d = provider.windows.iter().find(|w| {
                    let l = w.label.to_ascii_lowercase();
                    (l.contains("claude") || l.contains("gpt")) && (l.contains("7d") || w.kind == WindowKind::Weekly)
                });

                // Parent row
                let is_sel = app.limits_selected == s_idx;
                let ag_brand = provider_brand_color("antigravity");
                let cursor_mark = if is_sel { "▶ " } else { "  " };
                let mut title_style = Style::default().fg(ag_brand).add_modifier(Modifier::BOLD);
                if is_sel {
                    title_style = title_style.bg(Color::Rgb(28, 42, 60));
                }
                let mut parent_spans = vec![
                    Span::styled(cursor_mark, Style::default().fg(if is_sel { Color::Yellow } else { Color::Reset }).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("{} ", provider.availability.marker()),
                        if provider.availability == Availability::Available {
                            Style::default().fg(ag_brand)
                        } else {
                            status_style_for(provider)
                        },
                    ),
                    Span::styled(
                        fit(&provider_title_base(provider), cols.provider.saturating_sub(4)),
                        title_style,
                    ),
                    Span::raw("  "),
                    Span::styled(fit("2 pools active", cols.session), Style::default().fg(CYAN)),
                    Span::raw("  "),
                    Span::styled(fit("CLI (port 57388)", cols.cycle), Style::default().fg(DIM_GREY)),
                    Span::raw("  "),
                ];
                parent_spans.extend(format_dual_reset(gemini_5h, gemini_7d, cols.reset, now_ms, false));
                parent_spans.push(Span::raw("  "));
                parent_spans.push(format_matrix_status(provider, Some(("ready", GREEN)), cols.status, false));
                lines.push(Line::from(parent_spans));

                // Sub-row 1: Gemini Pool
                let gemini_color = Color::Rgb(66, 165, 245);
                let mut g_spans = vec![
                    Span::raw("  "),
                    Span::styled(fit("├─ Gemini Pool (Flash/Pro)", cols.provider.saturating_sub(2)), Style::default().fg(gemini_color).add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                ];
                let g_dim = gemini_5h.is_some_and(|w| w.remaining_percent == Some(0.0));
                g_spans.extend(format_quota_cell(gemini_5h, cols.bar_width, cols.session, gemini_color, g_dim));
                g_spans.push(Span::raw("  "));
                g_spans.extend(format_quota_cell(gemini_7d, cols.bar_width, cols.cycle, gemini_color, g_dim));
                g_spans.push(Span::raw("  "));
                g_spans.extend(format_dual_reset(gemini_5h, gemini_7d, cols.reset, now_ms, g_dim));
                g_spans.push(Span::raw("  "));
                let g_stat = if gemini_5h.is_some_and(|w| w.remaining_percent == Some(0.0)) {
                    ("5h capped", YELLOW)
                } else {
                    ("smooth", GREY)
                };
                g_spans.push(format_matrix_status(provider, Some(g_stat), cols.status, false));
                lines.push(Line::from(g_spans));

                // Sub-row 2: Claude / GPT Pool
                let claude_color = Color::Rgb(217, 119, 87);
                let mut c_spans = vec![
                    Span::raw("  "),
                    Span::styled(fit("└─ Claude/GPT Pool (Sonnet)", cols.provider.saturating_sub(2)), Style::default().fg(claude_color).add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                ];
                let c_dim = claude_5h.is_some_and(|w| w.remaining_percent == Some(0.0));
                c_spans.extend(format_quota_cell(claude_5h, cols.bar_width, cols.session, claude_color, c_dim));
                c_spans.push(Span::raw("  "));
                c_spans.extend(format_quota_cell(claude_7d, cols.bar_width, cols.cycle, claude_color, c_dim));
                c_spans.push(Span::raw("  "));
                c_spans.extend(format_dual_reset(claude_5h, claude_7d, cols.reset, now_ms, c_dim));
                c_spans.push(Span::raw("  "));
                let c_stat = if claude_5h.is_some_and(|w| w.remaining_percent == Some(100.0)) {
                    ("READY (FULL)", GREEN)
                } else if claude_5h.is_some_and(|w| w.remaining_percent == Some(0.0)) {
                    ("5h capped", YELLOW)
                } else {
                    ("ready", GREEN)
                };
                c_spans.push(format_matrix_status(provider, Some(c_stat), cols.status, false));
                lines.push(Line::from(c_spans));
                continue;
            }

            // Cursor Special Rendering
            if pid == "cursor" {
                let is_sel = app.limits_selected == s_idx;
                let cursor_models = provider.windows.iter().find(|w| w.label.to_ascii_lowercase().contains("cursor"));
                let other_models = provider.windows.iter().find(|w| w.label.to_ascii_lowercase().contains("other"));
                let brand = provider_brand_color("cursor");
                let cursor_mark = if is_sel { "▶ " } else { "  " };
                let mut title_style = Style::default().fg(brand).add_modifier(Modifier::BOLD);
                if is_sel {
                    title_style = title_style.bg(Color::Rgb(28, 42, 60));
                }
                let mut row = vec![
                    Span::styled(cursor_mark, Style::default().fg(if is_sel { Color::Yellow } else { Color::Reset }).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("{} ", provider.availability.marker()),
                        if provider.availability == Availability::Available {
                            Style::default().fg(brand)
                        } else {
                            status_style_for(provider)
                        },
                    ),
                    Span::styled(
                        fit(&provider_title_base(provider), cols.provider.saturating_sub(4)),
                        title_style,
                    ),
                    Span::raw("  "),
                ];
                let is_dimmed = provider.availability.dimmed() || provider.source_health != SourceHealth::Connected;
                row.extend(format_quota_cell(cursor_models, cols.bar_width, cols.session, brand, is_dimmed));
                row.push(Span::raw("  "));
                row.extend(format_quota_cell(other_models, cols.bar_width, cols.cycle, brand, is_dimmed));
                row.push(Span::raw("  "));
                row.extend(format_dual_reset(None, cursor_models.or(other_models), cols.reset, now_ms, is_dimmed));
                row.push(Span::raw("  "));
                row.push(format_matrix_status(provider, None, cols.status, false));
                lines.push(Line::from(row));
                continue;
            }

            // Standard Subscription (Claude, Codex, Command Code, Grok, etc.)
            let session_w = provider.windows.iter().find(|w| {
                !w.metric.is_credit() && (w.kind == WindowKind::Session || w.label.to_ascii_lowercase().contains("5h") || w.label.to_ascii_lowercase().contains("session"))
            });
            let cycle_w = provider.windows.iter().find(|w| {
                !w.metric.is_credit() && (w.kind == WindowKind::Weekly || w.kind == WindowKind::Monthly || w.kind == WindowKind::Daily || w.label.to_ascii_lowercase().contains("7d") || w.label.to_ascii_lowercase().contains("weekly") || w.label.to_ascii_lowercase().contains("daily"))
            });

            let is_sel = app.limits_selected == s_idx;
            let display_name = provider_title_base(provider);
            let brand = provider_brand_color(&provider.provider_id);
            let is_dimmed = provider.availability.dimmed() || provider.source_health != SourceHealth::Connected;
            let cursor_mark = if is_sel { "▶ " } else { "  " };
            let mut title_style = if is_dimmed {
                Style::default().fg(brand).add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(brand).add_modifier(Modifier::BOLD)
            };
            if is_sel {
                title_style = title_style.bg(Color::Rgb(28, 42, 60));
            }
            let mut row = vec![
                Span::styled(cursor_mark, Style::default().fg(if is_sel { Color::Yellow } else { Color::Reset }).add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("{} ", provider.availability.marker()),
                    if provider.availability == Availability::Available {
                        Style::default().fg(brand)
                    } else {
                        status_style_for(provider)
                    },
                ),
                Span::styled(
                    fit(&display_name, cols.provider.saturating_sub(4)),
                    title_style,
                ),
                Span::raw("  "),
            ];
            row.extend(format_quota_cell(session_w, cols.bar_width, cols.session, brand, is_dimmed));
            row.push(Span::raw("  "));
            row.extend(format_quota_cell(cycle_w, cols.bar_width, cols.cycle, brand, is_dimmed));
            row.push(Span::raw("  "));

            row.extend(format_dual_reset(session_w, cycle_w, cols.reset, now_ms, is_dimmed));
            row.push(Span::raw("  "));

            let is_burn_first = burn_rec.as_ref().is_some_and(|b| b.provider_title == display_name);
            let status_override = if session_w.is_some_and(|w| w.remaining_percent == Some(0.0)) && provider.availability == Availability::Available {
                Some(("5h capped", YELLOW))
            } else if cycle_w.is_some_and(|w| w.remaining_percent == Some(0.0)) && provider.availability == Availability::Available {
                Some(("weekly cap", RED))
            } else {
                None
            };
            row.push(format_matrix_status(provider, status_override, cols.status, is_burn_first));
            lines.push(Line::from(row));
        }
    }

    // Section 2: Credit Grants & Prepaid Wallets
    if !wallets.is_empty() {
        lines.push(Line::from(""));
        let banner_text = "── CREDIT GRANTS & PREPAID WALLETS ";
        let banner_pad = (width as usize).saturating_sub(str_width(banner_text));
        lines.push(Line::from(vec![
            Span::styled(banner_text, Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::styled("─".repeat(banner_pad), Style::default().fg(DIM_GREY)),
        ]));

        let (w_prov, w_bal, w_type, w_res) = if width >= 150 {
            (38, 22, 32, 18)
        } else if width >= 120 {
            (32, 18, 26, 15)
        } else if width >= 95 {
            (26, 16, 22, 13)
        } else {
            (20, 14, 18, 11)
        };
        let w_stat = (width as usize).saturating_sub(w_prov + w_bal + w_type + w_res + 8).max(10);

        lines.push(Line::from(vec![
            Span::styled(fit("PROVIDER / ACCOUNT", w_prov), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(fit("BALANCE", w_bal), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(fit("GRANT TYPE", w_type), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(fit("NEXT RESET", w_res), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(fit("STATUS", w_stat), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
        ]));

        lines.push(Line::from(Span::styled(
            "─".repeat(width as usize),
            Style::default().fg(DIM_GREY),
        )));

        for (w_idx, provider) in wallets.iter().enumerate() {
            let is_sel = app.limits_selected == subscriptions.len() + w_idx;
            let cursor_mark = if is_sel { "▶ " } else { "  " };
            let pid = provider.provider_id.to_ascii_lowercase();
            let is_modal = pid == "modal";

            let display_name = if is_modal {
                let acc = if !provider.account_label.is_empty() {
                    provider.account_label.as_str()
                } else {
                    "Default"
                };
                let clean_acc = acc.strip_prefix("Modal · ").unwrap_or(acc);
                format!("Modal · {}", clean_acc)
            } else if app.show_account && !provider.account_label.is_empty() {
                format!("{} · {}", provider_title_base(provider), provider.account_label)
            } else {
                provider_title_base(provider)
            };

            let balance_str = if let Some(w) = provider.windows.iter().find(|w| w.remaining_amount.is_some()) {
                let curr_raw = w.currency.as_deref().unwrap_or("$");
                let sym = match curr_raw.to_ascii_uppercase().as_str() {
                    "USD" | "$" => "$",
                    "CNY" | "RMB" | "¥" => "¥",
                    _ => curr_raw,
                };
                let amt = w.remaining_amount.unwrap_or(0.0);
                if let Some(pct) = w.remaining_percent {
                    format!("{sym}{amt:.2} ({pct:.0}%)")
                } else {
                    format!("{sym}{amt:.2}")
                }
            } else if let Some(w) = provider.windows.iter().find(|w| w.remaining_percent.is_some()) {
                format!("{:.1}%", w.remaining_percent.unwrap())
            } else {
                "—".to_owned()
            };

            let grant_type = if is_modal {
                "$30.00 Monthly Allowance".to_owned()
            } else if pid == "vast" || pid == "vastai" {
                "Prepaid Instance Credit".to_owned()
            } else {
                "Prepaid API Balance".to_owned()
            };

            let reset_str = if let Some(w) = provider.windows.iter().find(|w| w.resets_at_ms.is_some()) {
                let text = reset_text(w);
                if text.is_empty() || text == "—" {
                    "—".to_owned()
                } else {
                    format!("↻ {text}")
                }
            } else {
                "—".to_owned()
            };

            let (status_text, status_color) = if is_modal {
                ("active", GREEN)
            } else {
                ("PAYG", BLUE)
            };

            let brand = provider_brand_color(&provider.provider_id);

            let mut title_style = Style::default().fg(brand).add_modifier(Modifier::BOLD);
            if is_sel {
                title_style = title_style.bg(Color::Rgb(28, 42, 60));
            }
            lines.push(Line::from(vec![
                Span::styled(cursor_mark, Style::default().fg(if is_sel { Color::Yellow } else { Color::Reset }).add_modifier(Modifier::BOLD)),
                Span::styled("● ", Style::default().fg(brand)),
                Span::styled(
                    fit(&display_name, w_prov.saturating_sub(4)),
                    title_style,
                ),
                Span::raw("  "),
                Span::styled(
                    fit(&balance_str, w_bal),
                    Style::default().fg(brand).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(fit(&grant_type, w_type), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit(&reset_str, w_res), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit(status_text, w_stat), Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
            ]));
        }
    }

    // Section 3: Live Token Velocity & Daily Burn (Adaptive Bottom Panel for Tall/Fullscreen Panes)
    if height >= 28 && (subscriptions.len() + wallets.len()) <= 18 {
        if let Some(report) = &app.consumption {
            lines.push(Line::from(""));
            let banner_text = if width >= 120 {
                "── TOKEN CONSUMPTION & VELOCITY ── [Press '2' for Full Ledger] "
            } else {
                "── TOKEN VELOCITY ['2' for Ledger] "
            };
            let banner_pad = (width as usize).saturating_sub(str_width(banner_text));
            lines.push(Line::from(vec![
                Span::styled(banner_text, Style::default().fg(Color::Rgb(100, 180, 255)).add_modifier(Modifier::BOLD)),
                Span::styled("─".repeat(banner_pad), Style::default().fg(DIM_GREY)),
            ]));

            let temporal = if let Some(cached) = &app.cached_ledger {
                cached.buckets.clone()
            } else {
                compute_temporal_usage(report)
            };

            let col_w = if width >= 140 {
                18
            } else if width >= 115 {
                14
            } else {
                10
            };
            let win_w = if width >= 140 { 14 } else { 10 };

            lines.push(Line::from(vec![
                Span::styled(fit("WINDOW", win_w), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(fit("TOTAL TOKENS", col_w), Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(fit("INPUT", col_w), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(fit("OUTPUT", col_w), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(fit("CACHE", col_w), Style::default().fg(GREY).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(fit("API-EQ", 10), Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(fit("TOK/$ EFF", 10), Style::default().fg(Color::Rgb(66, 165, 245)).add_modifier(Modifier::BOLD)),
            ]));

            lines.push(Line::from(Span::styled("─".repeat(width as usize), Style::default().fg(DIM_GREY))));

            let max_buckets = if height >= 36 { 3 } else { 2 };
            for bucket in temporal.iter().take(max_buckets) {
                let cost_str = if bucket.api_usd > 0.0 {
                    format!("${:.2}", bucket.api_usd)
                } else {
                    "—".to_owned()
                };
                let eff_str = if bucket.api_usd > 0.0 {
                    format!("{:.2}M/$", bucket.tokens as f64 / 1_000_000.0 / bucket.api_usd)
                } else {
                    "—".to_owned()
                };
                lines.push(Line::from(vec![
                    Span::styled(fit(bucket.label, win_w), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(fit(&format_tokens(bucket.tokens), col_w), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(fit(&format_tokens(bucket.input), col_w), Style::default().fg(GREY)),
                    Span::raw("  "),
                    Span::styled(fit(&format_tokens(bucket.output), col_w), Style::default().fg(GREY)),
                    Span::raw("  "),
                    Span::styled(fit(&format_tokens(bucket.cache), col_w), Style::default().fg(Color::Rgb(100, 210, 106))),
                    Span::raw("  "),
                    Span::styled(fit(&cost_str, 10), Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(fit(&eff_str, 10), Style::default().fg(Color::Rgb(66, 165, 245))),
                ]));
            }
        }
    }

    lines
}

fn body_lines(app: &App, width: u16, height: u16) -> Vec<Line<'static>> {
    if app.view == View::Settings {
        return settings_lines(app, width);
    }
    if app.view == View::Consumption {
        return consumption_lines(app, width);
    }
    limits_matrix_lines(app, width, height)
}

fn print_usage(
    consumption: &usage::UsageSnapshot,
    json: bool,
    legacy: Option<&token_monitor_core::legacy::LegacyDailyTotals>,
    audit_pricing: bool,
) -> io::Result<()> {
    let report = usage::build_consumption_report_with_legacy(
        consumption.clone(),
        &token_monitor_core::pricing::PricingEngine::load_cached(),
        audit_pricing,
        legacy,
    );
    if json {
        let encoded = serde_json::to_string_pretty(&serde_json::json!({
            "version": "0.1.0-native",
            "summary": report.summary,
            "records": report.snapshot.records.len(),
            "clients": report.snapshot.clients(),
            "models": report.snapshot.models(),
            "processingTimeMs": report.snapshot.processing_time_ms,
            "tokscaleRevision": report.snapshot.tokscale_revision,
            "quotes": report.quotes,
        }))
        .map_err(|error| io::Error::other(format!("usage JSON encode failed: {error}")))?;
        println!("{encoded}");
        return Ok(());
    }
    let totals = report.snapshot.total_tokens();
    let summary = &report.summary;
    println!("TOKEN MONITOR · consumption · native Tokscale core");
    println!(
        "{} records · {} clients · {} models · parsed in {}ms",
        report.snapshot.records.len(),
        report.snapshot.clients().len(),
        report.snapshot.models().len(),
        report.snapshot.processing_time_ms
    );
    println!(
        "input {} · cache read {} · cache write {} · output {} · reasoning {}",
        totals.input, totals.cache_read, totals.cache_write, totals.output, totals.reasoning
    );
    println!(
        "API-equivalent {} · monitor-estimate {} · actual {}",
        summary
            .api_equivalent_usd
            .map(|value| format!("${value:.4}"))
            .unwrap_or_else(|| "—".into()),
        summary
            .monitor_estimate_usd
            .map(|value| format!("${value:.2}"))
            .unwrap_or_else(|| "—".into()),
        summary
            .actual_usd
            .map(|value| format!("${value:.2}"))
            .unwrap_or_else(|| "—".into()),
    );
    println!(
        "priced {} · unpriced {} · exact/partial/unknown {}/{}/{}",
        summary.priced_tokens,
        summary.unpriced_tokens,
        summary.exact_rows,
        summary.partial_rows,
        summary.unknown_rows
    );
    println!("Tokscale revision {}", report.snapshot.tokscale_revision);
    if !summary.pricing_warnings.is_empty() {
        println!("pricing warnings · {}", summary.pricing_warnings.join("; "));
    }
    let buckets = compute_temporal_usage(&report);
    println!();
    println!("── TOKEN USAGE BY TIME WINDOW (TODAY, 24H, 7D, 30D) ──────────────────────────────");
    println!(
        "{:<14} {:>14} {:>12} {:>12} {:>12} {:>12} {:>8}",
        "WINDOW", "TOTAL TOKENS", "INPUT", "OUTPUT", "CACHE", "API-EQ COST", "RECORDS"
    );
    println!("{}", "-".repeat(88));
    for bucket in buckets {
        let cost_str = if bucket.api_usd > 0.0 {
            format!("${:.2}", bucket.api_usd)
        } else {
            "—".to_owned()
        };
        println!(
            "{:<14} {:>14} {:>12} {:>12} {:>12} {:>12} {:>8}",
            bucket.label,
            format_tokens(bucket.tokens),
            format_tokens(bucket.input),
            format_tokens(bucket.output),
            format_tokens(bucket.cache),
            cost_str,
            bucket.records
        );
    }

    let rows = consumption_rows(&report, ConsumptionMetric::Tokens, LedgerWindow::AllTime, "");
    if !rows.is_empty() {
        println!();
        println!(
            "{:<28} {:>10} {:>12} {:>10} {:>6}",
            "SUBSCRIPTION / CLIENT", "TOKENS", "API-EQ", "PRICED", "ROWS"
        );
        println!("{}", "-".repeat(70));
        for row in &rows {
            let label = consumption_label(row);
            let tokens = format_tokens(row.token_total());
            let api_eq = if row.api_value_usd > 0.0 {
                format!("${:.2}", row.api_value_usd)
            } else {
                "—".to_owned()
            };
            let priced_pct = if row.token_total() > 0 {
                format!("{:.1}%", (row.priced_tokens as f64 / row.token_total() as f64 * 100.0).clamp(0.0, 100.0))
            } else {
                "—".to_owned()
            };
            println!(
                "{:<28} {:>10} {:>12} {:>10} {:>6}",
                fit(&label, 28).trim_end(),
                tokens,
                api_eq,
                priced_pct,
                row.records
            );
        }
    }
    Ok(())
}

fn public_snapshot(provider: &ProviderSnapshot, show_account: bool) -> serde_json::Value {
    let mut value = serde_json::to_value(provider).unwrap_or_else(|_| serde_json::json!({}));
    if !show_account {
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "accountLabel".into(),
                serde_json::Value::String(masked_account_label(provider)),
            );
            object.insert(
                "accountKey".into(),
                serde_json::Value::String("redacted".into()),
            );
        }
    }
    value
}

fn print_limits_json(
    providers: &[ProviderSnapshot],
    source: &str,
    show_account: bool,
) -> io::Result<()> {
    let public_providers = providers
        .iter()
        .map(|provider| public_snapshot(provider, show_account))
        .collect::<Vec<_>>();
    let encoded = serde_json::to_string_pretty(&serde_json::json!({
        "version": "0.1.0-foundation",
        "source": source,
        "providers": public_providers,
    }))
    .map_err(|error| io::Error::other(format!("limits JSON encode failed: {error}")))?;
    println!("{encoded}");
    Ok(())
}

fn import_legacy_history() {
    let Ok(storage) = token_monitor_core::storage::Storage::open_default() else {
        return;
    };
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let candidates = [
        home.join("Library/Application Support/Token Monitor/daily-history-archive.json"),
        home.join("Downloads/git/token-monitor-tui/subscription-limit-history.jsonl"),
        home.join("Downloads/git/token-monitor-tui/grok-build-limit-history.jsonl"),
    ];
    let now = chrono::Utc::now().timestamp_millis();
    for path in candidates {
        if path.is_file() {
            let _ = storage.import_legacy_json(&path, now);
        }
    }
}

fn legacy_totals_for_range(
    since: Option<&str>,
    until: Option<&str>,
) -> Option<token_monitor_core::legacy::LegacyDailyTotals> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let path = home.join("Library/Application Support/Token Monitor/daily-history-archive.json");
    if let Ok(storage) = token_monitor_core::storage::Storage::open_default() {
        if let Ok(Some(payload)) = storage.imported_payload(&path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload) {
                if let Ok(totals) = token_monitor_core::legacy::summarize_daily_archive_value(
                    &value,
                    &path.to_string_lossy(),
                    since,
                    until,
                ) {
                    return Some(totals);
                }
            }
        }
    }
    token_monitor_core::legacy::read_daily_archive(&path, since, until).ok()
}

fn load_cached_app(
    no_color: bool,
    show_account: bool,
    selected_providers: Option<&[String]>,
) -> App {
    let Ok(storage) = token_monitor_core::storage::Storage::open_default() else {
        let mut app = App::new(no_color, show_account);
        app.live = true;
        app.providers.clear();
        return app;
    };
    let mut app = match storage.latest_provider_snapshots() {
        Ok(mut providers) if !providers.is_empty() => {
            let selected = selected_providers.map(|values| {
                values
                    .iter()
                    .map(|value| value.to_ascii_lowercase())
                    .collect::<HashSet<_>>()
            });
            let show_all = selected
                .as_ref()
                .is_some_and(|values| values.contains("all"));
            let selected = selected.as_ref();
            // A registry placeholder can remain in SQLite after an adapter is
            // added. Prefer the real row and avoid showing a duplicate stale
            // `Kiro`/similar entry during the instant first paint.
            let implemented = providers
                .iter()
                .filter(|provider| provider.source != "registry")
                .map(|provider| provider.provider_id.clone())
                .collect::<HashSet<_>>();
            providers.retain(|provider| {
                let requested = show_all
                    || selected.is_none_or(|values| values.contains(&provider.provider_id));
                let configured = selected.is_some() || provider.visible_by_default();
                requested
                    && configured
                    && !(provider.source == "registry"
                        && implemented.contains(&provider.provider_id))
            });
            // The cache is useful for instant first paint, but it is not a live
            // answer until the background collector completes.
            for provider in &mut providers {
                if provider.source_health == SourceHealth::Connected {
                    provider.source_health = SourceHealth::Stale;
                }
            }
            App::from_providers(providers, no_color, show_account)
        }
        _ => {
            let mut app = App::new(no_color, show_account);
            app.live = true;
            app.providers.clear();
            app
        }
    };
    if let Ok(Some(snapshot)) = storage.latest_usage_snapshot() {
        let report = usage::build_consumption_report_with_legacy(
            snapshot,
            &token_monitor_core::pricing::PricingEngine::load_cached(),
            true,
            // The cached UsageSnapshot does not encode its original date
            // range. Do not pair it with an all-history legacy total; the
            // first background refresh will attach the requested range.
            None,
        );
        app.set_consumption(report);
    }
    app
}


fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn modal_lines(modal: &DetailModal, width: u16) -> Vec<Line<'static>> {
    let max_w = (width as usize).saturating_sub(4);
    match modal {
        DetailModal::None => vec![],
        DetailModal::LimitsProvider(p) => {
            let mut lines = Vec::new();
            let brand = provider_brand_color(&p.provider_id);
            lines.push(Line::from(vec![
                Span::styled(format!("PROVIDER: {} ", p.provider_id.to_uppercase()), Style::default().fg(brand).add_modifier(Modifier::BOLD)),
                Span::raw(" · Plan: "),
                Span::styled(p.plan.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::raw(" · Status: "),
                Span::styled(format!("{:?}", p.availability), Style::default().fg(if p.availability == Availability::Available { GREEN } else { RED })),
            ]));
            lines.push(Line::from(Span::styled(format!("Account: {}", p.account_label), Style::default().fg(GREY))));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("RATE LIMIT WINDOWS:", Style::default().fg(GREY).add_modifier(Modifier::BOLD))));
            for w in &p.windows {
                let r_text = reset_text(w);
                let pct_text = w.remaining_percent.map(|pct| format!("{:>5.1}%", pct)).unwrap_or_else(|| "   — ".into());
                let amt_text = w.remaining_amount.map(|amt| format!("{}{:.2}", w.currency.as_deref().unwrap_or("$"), amt)).unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(fit(&format!("  • [{:?}] {}", w.kind, w.label), 24), Style::default().fg(Color::White)),
                    Span::raw(" "),
                    Span::styled(pct_text, Style::default().fg(brand).add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(fit(&amt_text, 10), Style::default().fg(YELLOW)),
                    Span::raw("  resets in "),
                    Span::styled(r_text, Style::default().fg(CYAN)),
                ]));
            }
            if !p.diagnostics.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("DIAGNOSTICS & DETAILS:", Style::default().fg(GREY).add_modifier(Modifier::BOLD))));
                for d in &p.diagnostics {
                    lines.push(Line::from(Span::styled(format!("  • {}", fit(d, max_w)), Style::default().fg(GREY))));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("[Esc / Enter / Space to close]", Style::default().fg(GREY).add_modifier(Modifier::DIM))));
            lines
        }
        DetailModal::ConsumptionWindow { window, records, tokens, api_usd, clients, models } => {
            let mut lines = Vec::new();
            let grand = tokens.reported_total_without_reasoning();
            let eff = if *api_usd > 0.0 {
                format!("{:.2}M/$", grand as f64 / 1_000_000.0 / api_usd)
            } else {
                "—".to_owned()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("TIME WINDOW: {} ", window.label().to_uppercase()), Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
                Span::raw(" · "),
                Span::styled(format_tokens(grand), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled(" tokens", Style::default().fg(GREY)),
                Span::raw(" · API-EQ "),
                Span::styled(format!("${:.2}", api_usd), Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
                Span::raw(" · Eff: "),
                Span::styled(eff, Style::default().fg(Color::Rgb(66, 165, 245)).add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::from(Span::styled(format!("Total logged requests in this window: {}", records), Style::default().fg(GREY))));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("TOKEN TYPE BREAKDOWN:", Style::default().fg(GREY).add_modifier(Modifier::BOLD))));
            lines.push(Line::from(vec![
                Span::styled("  Input: ", Style::default().fg(GREY)),
                Span::styled(format_tokens(tokens.input), Style::default().fg(Color::White)),
                Span::raw("    Output: "),
                Span::styled(format_tokens(tokens.output), Style::default().fg(Color::White)),
                Span::raw("    Cache Read: "),
                Span::styled(format_tokens(tokens.cache_read), Style::default().fg(Color::White)),
                Span::raw("    Reasoning: "),
                Span::styled(format_tokens(tokens.reasoning), Style::default().fg(Color::White)),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("CLIENT / HARNESS & SUB-MODEL HIERARCHY (PARENT ➔ CHILDREN):", Style::default().fg(GREY).add_modifier(Modifier::BOLD))));
            lines.push(Line::from(vec![
                Span::styled(fit("CLIENT / SUB-MODEL", 32), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit("TOKENS", 12), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit("API-EQ", 10), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit("TOK/$", 10), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit("CALLS", 8), Style::default().fg(GREY)),
            ]));
            lines.push(Line::from(Span::styled("─".repeat(max_w.min(78)), Style::default().fg(DIM_GREY))));
            for c_item in clients.iter().take(6) {
                let brand = client_brand_color(&c_item.client);
                let c_eff = if c_item.api_usd > 0.0 { format!("{:.2}M/$", c_item.tokens as f64 / 1_000_000.0 / c_item.api_usd) } else { "—".to_owned() };
                let val_str = if c_item.api_usd > 0.0 { format!("${:.2}", c_item.api_usd) } else { "—".to_owned() };
                lines.push(Line::from(vec![
                    Span::styled(fit(&format!("▼ {}", c_item.client), 32), Style::default().fg(brand).add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(fit(&format_tokens(c_item.tokens), 12), Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(fit(&val_str, 10), Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(fit(&c_eff, 10), Style::default().fg(Color::Rgb(66, 165, 245))),
                    Span::raw("  "),
                    Span::styled(fit(&c_item.calls.to_string(), 8), Style::default().fg(DIM_GREY)),
                ]));
                for (sub_m, s_tok, s_usd, s_calls) in c_item.models.iter().take(5) {
                    let s_eff = if *s_usd > 0.0 { format!("{:.2}M/$", *s_tok as f64 / 1_000_000.0 / s_usd) } else { "—".to_owned() };
                    let s_val_str = if *s_usd > 0.0 { format!("${:.2}", s_usd) } else { "—".to_owned() };
                    let m_hue = sub_m.bytes().fold(0u8, |s, b| s.wrapping_add(b));
                    lines.push(Line::from(vec![
                        Span::styled(fit(&format!("  └─ {}", sub_m), 32), Style::default().fg(palette(m_hue))),
                        Span::raw("  "),
                        Span::styled(fit(&format_tokens(*s_tok), 12), Style::default().fg(CYAN)),
                        Span::raw("  "),
                        Span::styled(fit(&s_val_str, 10), Style::default().fg(YELLOW)),
                        Span::raw("  "),
                        Span::styled(fit(&s_eff, 10), Style::default().fg(Color::Rgb(66, 165, 245))),
                        Span::raw("  "),
                        Span::styled(fit(&s_calls.to_string(), 8), Style::default().fg(DIM_GREY)),
                    ]));
                }
            }
            if !models.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("TOP MODELS IN THIS WINDOW (WITH PARENT HARNESS):", Style::default().fg(GREY).add_modifier(Modifier::BOLD))));
                lines.push(Line::from(vec![
                    Span::styled(fit("MODEL", 26), Style::default().fg(GREY)),
                    Span::raw("  "),
                    Span::styled(fit("PARENT HARNESS", 18), Style::default().fg(GREY)),
                    Span::raw("  "),
                    Span::styled(fit("TOKENS", 12), Style::default().fg(GREY)),
                    Span::raw("  "),
                    Span::styled(fit("API-EQ", 10), Style::default().fg(GREY)),
                    Span::raw("  "),
                    Span::styled(fit("TOK/$", 10), Style::default().fg(GREY)),
                    Span::raw("  "),
                    Span::styled(fit("CALLS", 6), Style::default().fg(GREY)),
                ]));
                lines.push(Line::from(Span::styled("─".repeat(max_w.min(90)), Style::default().fg(DIM_GREY))));
                for (m, parent_harness, tok, val, calls) in models.iter().take(8) {
                    let m_eff = if *val > 0.0 { format!("{:.2}M/$", *tok as f64 / 1_000_000.0 / val) } else { "—".to_owned() };
                    let val_str = if *val > 0.0 { format!("${:.2}", val) } else { "—".to_owned() };
                    let hue = m.bytes().fold(0u8, |s, b| s.wrapping_add(b));
                    let h_brand = client_brand_color(parent_harness);
                    lines.push(Line::from(vec![
                        Span::styled(fit(m, 26), Style::default().fg(palette(hue))),
                        Span::raw("  "),
                        Span::styled(fit(parent_harness, 18), Style::default().fg(h_brand)),
                        Span::raw("  "),
                        Span::styled(fit(&format_tokens(*tok), 12), Style::default().fg(CYAN)),
                        Span::raw("  "),
                        Span::styled(fit(&val_str, 10), Style::default().fg(YELLOW)),
                        Span::raw("  "),
                        Span::styled(fit(&m_eff, 10), Style::default().fg(Color::Rgb(66, 165, 245))),
                        Span::raw("  "),
                        Span::styled(fit(&calls.to_string(), 6), Style::default().fg(DIM_GREY)),
                    ]));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("[Esc / Enter / Space to close · ↑/↓/j/k to scroll · press 'w' in ledger to switch active window]", Style::default().fg(GREY).add_modifier(Modifier::DIM))));
            lines
        }
        DetailModal::ConsumptionClient { client, window, records, tokens, api_usd, models } => {
            let mut lines = Vec::new();
            let grand = tokens.reported_total_without_reasoning();
            let eff = if *api_usd > 0.0 {
                format!("{:.2}M/$", grand as f64 / 1_000_000.0 / api_usd)
            } else {
                "—".to_owned()
            };
            let brand = client_brand_color(client);
            lines.push(Line::from(vec![
                Span::styled(format!("CLIENT / HARNESS: {} ", client.to_uppercase()), Style::default().fg(brand).add_modifier(Modifier::BOLD)),
                Span::raw(" · "),
                Span::styled(format_tokens(grand), Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" tokens ({})", window.label()), Style::default().fg(GREY)),
                Span::raw(" · API-EQ "),
                Span::styled(format!("${:.2}", api_usd), Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
                Span::raw(" · Eff: "),
                Span::styled(eff, Style::default().fg(Color::Rgb(66, 165, 245)).add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::from(Span::styled(format!("Total logged requests in this window: {}", records), Style::default().fg(GREY))));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("TOKEN TYPE BREAKDOWN:", Style::default().fg(GREY).add_modifier(Modifier::BOLD))));
            lines.push(Line::from(vec![
                Span::styled("  Input: ", Style::default().fg(GREY)),
                Span::styled(format_tokens(tokens.input), Style::default().fg(Color::White)),
                Span::raw("    Output: "),
                Span::styled(format_tokens(tokens.output), Style::default().fg(Color::White)),
                Span::raw("    Cache Read: "),
                Span::styled(format_tokens(tokens.cache_read), Style::default().fg(Color::White)),
                Span::raw("    Reasoning: "),
                Span::styled(format_tokens(tokens.reasoning), Style::default().fg(Color::White)),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("MODELS CALLED BY THIS CLIENT:", Style::default().fg(GREY).add_modifier(Modifier::BOLD))));
            lines.push(Line::from(vec![
                Span::styled(fit("MODEL", 28), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit("TOKENS", 12), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit("API-EQ", 10), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit("TOK/$", 10), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit("CALLS", 8), Style::default().fg(GREY)),
            ]));
            lines.push(Line::from(Span::styled("─".repeat(max_w.min(74)), Style::default().fg(DIM_GREY))));
            for (m, tok, val, calls) in models.iter().take(8) {
                let m_eff = if *val > 0.0 { format!("{:.2}M/$", *tok as f64 / 1_000_000.0 / val) } else { "—".to_owned() };
                let val_str = if *val > 0.0 { format!("${:.2}", val) } else { "—".to_owned() };
                let hue = m.bytes().fold(0u8, |s, b| s.wrapping_add(b));
                lines.push(Line::from(vec![
                    Span::styled(fit(m, 28), Style::default().fg(palette(hue))),
                    Span::raw("  "),
                    Span::styled(fit(&format_tokens(*tok), 12), Style::default().fg(CYAN)),
                    Span::raw("  "),
                    Span::styled(fit(&val_str, 10), Style::default().fg(YELLOW)),
                    Span::raw("  "),
                    Span::styled(fit(&m_eff, 10), Style::default().fg(Color::Rgb(66, 165, 245))),
                    Span::raw("  "),
                    Span::styled(fit(&calls.to_string(), 8), Style::default().fg(DIM_GREY)),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("[Esc / Enter / Space to close]", Style::default().fg(GREY).add_modifier(Modifier::DIM))));
            lines
        }
        DetailModal::ConsumptionModel { model, window, records, tokens, api_usd, clients } => {
            let mut lines = Vec::new();
            let grand = tokens.reported_total_without_reasoning();
            let eff = if *api_usd > 0.0 {
                format!("{:.2}M/$", grand as f64 / 1_000_000.0 / api_usd)
            } else {
                "—".to_owned()
            };
            let hue = model.bytes().fold(0u8, |s, b| s.wrapping_add(b));
            lines.push(Line::from(vec![
                Span::styled(format!("MODEL: {} ", model), Style::default().fg(palette(hue)).add_modifier(Modifier::BOLD)),
                Span::raw(" · "),
                Span::styled(format_tokens(grand), Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" tokens ({})", window.label()), Style::default().fg(GREY)),
                Span::raw(" · API-EQ "),
                Span::styled(format!("${:.2}", api_usd), Style::default().fg(YELLOW).add_modifier(Modifier::BOLD)),
                Span::raw(" · Eff: "),
                Span::styled(eff, Style::default().fg(Color::Rgb(66, 165, 245)).add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::from(Span::styled(format!("Total logged requests in this window: {}", records), Style::default().fg(GREY))));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("TOKEN TYPE BREAKDOWN:", Style::default().fg(GREY).add_modifier(Modifier::BOLD))));
            lines.push(Line::from(vec![
                Span::styled("  Input: ", Style::default().fg(GREY)),
                Span::styled(format_tokens(tokens.input), Style::default().fg(Color::White)),
                Span::raw("    Output: "),
                Span::styled(format_tokens(tokens.output), Style::default().fg(Color::White)),
                Span::raw("    Cache Read: "),
                Span::styled(format_tokens(tokens.cache_read), Style::default().fg(Color::White)),
                Span::raw("    Reasoning: "),
                Span::styled(format_tokens(tokens.reasoning), Style::default().fg(Color::White)),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("CLIENTS / HARNESSES INVOKING THIS MODEL:", Style::default().fg(GREY).add_modifier(Modifier::BOLD))));
            lines.push(Line::from(vec![
                Span::styled(fit("CLIENT", 24), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit("TOKENS", 12), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit("API-EQ", 10), Style::default().fg(GREY)),
                Span::raw("  "),
                Span::styled(fit("CALLS", 8), Style::default().fg(GREY)),
            ]));
            lines.push(Line::from(Span::styled("─".repeat(max_w.min(60)), Style::default().fg(DIM_GREY))));
            for (c, tok, val, calls) in clients.iter().take(8) {
                let brand = client_brand_color(c);
                let val_str = if *val > 0.0 { format!("${:.2}", val) } else { "—".to_owned() };
                lines.push(Line::from(vec![
                    Span::styled(fit(c, 24), Style::default().fg(brand).add_modifier(Modifier::BOLD)),
                    Span::raw("  "),
                    Span::styled(fit(&format_tokens(*tok), 12), Style::default().fg(CYAN)),
                    Span::raw("  "),
                    Span::styled(fit(&val_str, 10), Style::default().fg(YELLOW)),
                    Span::raw("  "),
                    Span::styled(fit(&calls.to_string(), 8), Style::default().fg(DIM_GREY)),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("[Esc / Enter / Space to close]", Style::default().fg(GREY).add_modifier(Modifier::DIM))));
            lines
        }
    }
}
fn draw(frame: &mut Frame, app: &App, forced_width: Option<u16>, refresh_seconds: u64) {
    let area = frame.area();
    let width = forced_width.unwrap_or(area.width).max(30);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let header = header_lines(app, width, refresh_seconds);
    frame.render_widget(Clear, chunks[0]);
    frame.render_widget(Paragraph::new(header), chunks[0]);
    frame.render_widget(Clear, chunks[1]);
    let body_rendered = body_lines(app, width, area.height);
    let total_lines = body_rendered.len() as u16;
    let viewport_height = chunks[1].height;

    let cursor_pos = body_rendered
        .iter()
        .position(|line| line.spans.iter().any(|s| s.content.contains('▶') || s.content.contains('›')));

    let effective_scroll = if total_lines <= viewport_height {
        0
    } else {
        let mut s = app.scroll;
        if let Some(c_line) = cursor_pos {
            let cursor_line = c_line as u16;
            if cursor_line < s {
                s = cursor_line;
            } else if cursor_line >= s.saturating_add(viewport_height) {
                s = cursor_line.saturating_add(1).saturating_sub(viewport_height);
            }
        }
        let max_scroll = total_lines.saturating_sub(viewport_height);
        s.min(max_scroll)
    };

    let body = Paragraph::new(body_rendered).scroll((effective_scroll, 0));
    frame.render_widget(body, chunks[1]);
    let _filter = match app.filter {
        Filter::All => "all",
        Filter::Attention => "attention",
        Filter::Credits => "credits",
        Filter::Quotas => "quotas",
    };
    let _mode = if app.live {
        "live"
    } else if app.refreshes == 0 {
        "fixture"
    } else {
        "refreshed"
    };
    let footer = if app.searching {
        format!("  FILTER: /{}█  (Enter to lock · Esc to cancel)", app.search_query)
    } else if !app.search_query.is_empty() {
        if width < 110 {
            format!("[1] Limits  [2] Ledger  [3] Config │ Filter: \"{}\"*  [Esc] Clear │ [Enter] Detail  [q] Quit", app.search_query)
        } else {
            format!("[1] Limits  [2] Ledger  [3] Config │ Filter: \"{}\"*  [Esc] Clear  [/] Edit │ [j/k] Select  [Enter] Detail  [q] Quit", app.search_query)
        }
    } else if width < 110 {
        if app.view == View::Consumption {
            format!("[1] Limits  [2] Ledger*  [3] Config │ [h/l] Win:{}  [r] Ref  [Enter] Detail  [q] Quit", app.ledger_window.label())
        } else if app.view == View::Settings {
            "[1] Limits  [2] Ledger  [3] Config* │ [Enter] Edit  [r] Ref  [Esc] Back  [q] Quit".to_owned()
        } else {
            "[1] Limits*  [2] Ledger  [3] Config │ [/] Filter  [Enter] Detail  [r] Ref  [q] Quit".to_owned()
        }
    } else {
        if app.view == View::Consumption {
            format!("[1] Limits  [2] Ledger*  [3] Config │ [h/l / ←/→] Window: {}  [/] Filter  [m] Metric  [Enter] Detail  [r] Refresh  [q] Quit", app.ledger_window.label())
        } else if app.view == View::Settings {
            "[1] Limits  [2] Ledger  [3] Config* │ [Enter] Edit  [d] Delete  [r] Refresh  [Esc] Back  [q] Quit".to_owned()
        } else {
            "[1] Limits*  [2] Ledger  [3] Config │ [/] Filter  [j/k] Select  [Enter] Detail  [a] Attn  [c] Credits  [r] Refresh  [q] Quit".to_owned()
        }
    };
    frame.render_widget(Clear, chunks[2]);
    frame.render_widget(
        Paragraph::new(Span::styled(footer, Style::default().fg(GREY))),
        chunks[2],
    );
    if app.modal != DetailModal::None {
        let popup_area = centered_rect(88, 86, area);
        frame.render_widget(Clear, popup_area);
        let m_lines = modal_lines(&app.modal, popup_area.width);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD))
            .title(" DETAIL DRILL-DOWN ");
        let paragraph = Paragraph::new(m_lines).scroll((app.modal_scroll, 0)).block(block);
        frame.render_widget(paragraph, popup_area);
    }
    if app.no_color {
        for cell in &mut frame.buffer_mut().content {
            cell.set_style(Style::default());
        }
    }
}

fn run_once(args: &Args, app: &App) {
    let term_w = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(100);
    let env_cols = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<u16>().ok());
    let width = args.width.or(env_cols).unwrap_or(term_w).max(40);
    let required_height = (body_lines(app, width, 100).len() as u16 + 5).max(24);
    let height = std::env::var("LINES")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| *value >= 8)
        .unwrap_or(required_height)
        .max(required_height);
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test backend");
    terminal
        .draw(|frame| draw(frame, app, Some(width), args.refresh))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    for row in 0..buffer.area.height {
        let text = (0..buffer.area.width)
            .map(|column| buffer[(column, row)].symbol())
            .collect::<String>();
        println!("{}", text.trim_end());
    }
}

async fn run_interactive(args: &Args, mut app: App) -> io::Result<()> {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::Show
        );
        default_panic(panic_info);
    }));
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let (refresh_tx, refresh_rx) = std::sync::mpsc::channel::<Vec<ProviderSnapshot>>();
    let (usage_tx, usage_rx) =
        std::sync::mpsc::channel::<Result<usage::ConsumptionReport, String>>();
    let mut refresh_in_flight = false;
    let mut usage_in_flight = false;
    let mut next_refresh = Instant::now();
    let mut next_usage_refresh = Instant::now();
    let mut dirty = true;
    let collector_options =
        args.providers
            .as_ref()
            .map(|values| token_monitor_core::collectors::CollectorOptions {
                providers: Some(
                    values
                        .iter()
                        .map(|value| value.to_ascii_lowercase())
                        .collect(),
                ),
                ..Default::default()
            });
    let result = (|| -> io::Result<()> {
        loop {
            let mut changed = false;
            if let Ok(providers) = refresh_rx.try_recv() {
                app.providers =
                    token_monitor_core::merge_provider_snapshots(&app.providers, providers);
                sort_burn_first(&mut app.providers, chrono::Utc::now().timestamp_millis());
                app.live = true;
                refresh_in_flight = false;
                app.limits_busy = false;
                if let Ok(storage) = token_monitor_core::storage::Storage::open_default() {
                    let _ = storage.save_provider_snapshots(&app.providers);
                }
                changed = true;
            }
            if let Ok(result) = usage_rx.try_recv() {
                usage_in_flight = false;
                app.usage_busy = false;
                if let Ok(report) = result {
                    if let Ok(storage) = token_monitor_core::storage::Storage::open_default() {
                        let _ = storage.save_usage_snapshot(
                            &report.snapshot,
                            chrono::Utc::now().timestamp_millis(),
                        );
                    }
                    app.set_consumption(report);
                }
                changed = true;
            }
            if args.live && !refresh_in_flight && Instant::now() >= next_refresh {
                refresh_in_flight = true;
                app.limits_busy = true;
                let tx = refresh_tx.clone();
                let options = collector_options.clone().unwrap_or_default();
                tokio::spawn(async move {
                    let timeout = options.timeout();
                    let providers = match tokio::time::timeout(
                        timeout + Duration::from_secs(2),
                        token_monitor_core::collectors::collect_live_limits(&options),
                    )
                    .await
                    {
                        Ok(p) => p,
                        Err(_) => Vec::new(),
                    };
                    let _ = tx.send(providers);
                });
                next_refresh = Instant::now() + app.refresh_seconds(args);
                changed = true;
            }
            if args.live
                && app.view == View::Consumption
                && !usage_in_flight
                && Instant::now() >= next_usage_refresh
            {
                usage_in_flight = true;
                app.usage_busy = true;
                let tx = usage_tx.clone();
                let days = args.days;
                tokio::task::spawn_blocking(move || {
                    let since = (days > 0).then(|| {
                        (chrono::Utc::now() - chrono::Duration::days(days as i64))
                            .date_naive()
                            .to_string()
                    });
                    let result = usage::collect_local_usage(usage::UsageOptions {
                        clients: None,
                        since,
                        ..usage::UsageOptions::default()
                    })
                    .map(|snapshot| {
                        let legacy = legacy_totals_for_range(
                            (days > 0)
                                .then(|| {
                                    (chrono::Utc::now() - chrono::Duration::days(days as i64))
                                        .date_naive()
                                        .to_string()
                                 })
                                .as_deref(),
                            None,
                        );
                        usage::build_consumption_report_with_legacy(
                            snapshot,
                            &token_monitor_core::pricing::PricingEngine::load_cached(),
                            true,
                            legacy.as_ref(),
                        )
                    });
                    let _ = tx.send(result);
                });
                next_usage_refresh = Instant::now() + Duration::from_secs(300);
                changed = true;
            }
            let active_busy = match app.view {
                View::Consumption => app.usage_busy,
                _ => app.limits_busy,
            };
            if active_busy || app.last_refresh.elapsed() < Duration::from_millis(1600) {
                dirty = true;
            }
            if changed {
                dirty = true;
            }
            if app.should_quit {
                break Ok(());
            }
            if dirty {
                terminal.draw(|frame| draw(frame, &app, args.width, args.refresh))?;
                dirty = false;
            }
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) => {
                        let immediate_refresh = matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R'));
                        app.handle_key(key);
                        if immediate_refresh {
                            app.last_refresh = Instant::now();
                            app.refreshes = app.refreshes.saturating_add(1);
                            if app.view == View::Consumption {
                                next_usage_refresh = Instant::now();
                                app.usage_busy = true;
                            } else {
                                next_refresh = Instant::now();
                                app.limits_busy = true;
                            }
                        }
                        if key.code == KeyCode::Char('t') && app.view == View::Consumption {
                            next_usage_refresh = Instant::now();
                        }
                        dirty = true;
                    }
                    Event::Paste(text) => {
                        app.handle_paste(&text);
                        dirty = true;
                    }
                    Event::Resize(_cols, _rows) => {
                        let _ = terminal.autoresize();
                        let _ = terminal.clear();
                        dirty = true;
                    }
                    _ => {}
                }
            }
            if app.refresh_requested {
                app.refresh_requested = false;
                next_refresh = Instant::now();
                dirty = true;
            }
        }
    })();
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = Args::parse();
    args.resolve_live();
    if !args.skip_import {
        import_legacy_history();
    }
    if args.consumption {
        let since = (args.days > 0).then(|| {
            // Match the Node ledger's `--days N` convention: include today and
            // the calendar date exactly N days before today.
            (chrono::Utc::now() - chrono::Duration::days(args.days as i64))
                .date_naive()
                .to_string()
        });
        let snapshot = usage::collect_local_usage(usage::UsageOptions {
            clients: args.clients.clone(),
            since: since.clone(),
            ..usage::UsageOptions::default()
        })
        .map_err(io::Error::other)?;
        if let Ok(storage) = token_monitor_core::storage::Storage::open_default() {
            let _ = storage.save_usage_snapshot(&snapshot, chrono::Utc::now().timestamp_millis());
        }
        let legacy = legacy_totals_for_range(since.as_deref(), None);
        return print_usage(&snapshot, args.json, legacy.as_ref(), args.audit_pricing);
    }
    let interactive_live = args.live && !args.once && io::stdout().is_terminal();
    let mut app = if interactive_live {
        load_cached_app(args.no_color, args.show_account, args.providers.as_deref())
    } else if args.live {
        let providers = args.providers.as_ref().map(|values| {
            values
                .iter()
                .map(|value| value.to_ascii_lowercase())
                .collect()
        });
        let providers = token_monitor_core::collectors::collect_live_limits(
            &token_monitor_core::collectors::CollectorOptions {
                providers,
                ..Default::default()
            },
        )
        .await;
        let app = App::from_providers(providers, args.no_color, args.show_account);
        if let Ok(storage) = token_monitor_core::storage::Storage::open_default() {
            let _ = storage.save_provider_snapshots(&app.providers);
        }
        app
    } else {
        App::new(args.no_color, args.show_account)
    };
    if args.json {
        sort_burn_first(&mut app.providers, chrono::Utc::now().timestamp_millis());
        return print_limits_json(
            &app.providers,
            if args.live { "live" } else { "fixture" },
            args.show_account,
        );
    }
    if args.once || !io::stdout().is_terminal() {
        run_once(&args, &app);
        return Ok(());
    }
    run_interactive(&args, app).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_toggle_back_to_all() {
        let mut app = App::new(true, false);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(app.filter, Filter::Credits);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(app.filter, Filter::All);
    }

    #[test]
    fn t_toggles_consumption_view() {
        let mut app = App::new(true, false);
        assert_eq!(app.view, View::Limits);
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(app.view, View::Consumption);
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(app.view, View::Limits);
    }

    #[test]
    fn m_toggles_consumption_rank_metric() {
        let mut app = App::new(true, false);
        app.view = View::Consumption;
        assert_eq!(app.consumption_metric, ConsumptionMetric::Tokens);
        app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert_eq!(app.consumption_metric, ConsumptionMetric::ApiEquivalent);
        app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        assert_eq!(app.consumption_metric, ConsumptionMetric::Tokens);
    }

    #[test]
    fn settings_screen_supports_masked_paste_without_auto_saving() {
        let mut app = App::new(true, false);
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(app.view, View::Settings);
        app.settings.selected = app
            .settings
            .provider_ids
            .iter()
            .position(|provider| provider == "claude")
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.settings.editing);
        app.handle_paste("secret-value\n");
        assert_eq!(app.settings.input, "secret-value");
        let text = settings_lines(&app, 100)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Claude"));
        assert!(text.contains("provider page"));
        assert!(text.contains("••••••••••••"));
    }

    #[test]
    fn test_backend_frame_fits_narrow_width() {
        let app = App::new(true, false);
        for width in [40u16, 60, 80, 120, 160] {
            let backend = ratatui::backend::TestBackend::new(width, 24);
            let mut terminal = Terminal::new(backend).expect("test backend");
            terminal
                .draw(|frame| draw(frame, &app, Some(width), 60))
                .expect("draw");
            for line in terminal.backend().buffer().content.chunks(width as usize) {
                assert_eq!(line.len(), width as usize);
            }
        }
    }

    #[test]
    fn limits_does_not_scroll_when_all_content_fits() {
        let mut app = App::new(true, false);
        app.limits_selected = 10;
        let backend = ratatui::backend::TestBackend::new(99, 26);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal.draw(|frame| draw(frame, &app, Some(99), 60)).expect("draw");
        let mut full_text = String::new();
        for y in 0..26 {
            for x in 0..99 {
                full_text.push_str(terminal.backend().buffer().get(x, y).symbol());
            }
            full_text.push('\n');
        }
        assert!(full_text.contains("AI SUBSCRIPTIONS"));
        assert!(full_text.contains("Vast.ai"));
    }

    #[test]
    fn consumption_view_contains_both_value_measures() {
        let mut app = App::new(true, false);
        app.view = View::Consumption;
        let pricing = token_monitor_core::pricing::PricingEngine::load_cached();
        let report = usage::build_consumption_report(usage::UsageSnapshot::default(), &pricing, true);
        app.set_consumption(report);
        let lines = body_lines(&app, 100, 26);
        let text = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("API-EQ"));
    }

    #[test]
    fn cursor_navigation_and_horizontal_window_switching() {
        let mut app = App::new(true, false);
        app.view = View::Consumption;
        let pricing = token_monitor_core::pricing::PricingEngine::load_cached();
        let report = usage::build_consumption_report(usage::UsageSnapshot::default(), &pricing, true);
        app.set_consumption(report);

        // Initial window set to Today, cursor at 0
        app.ledger_window = LedgerWindow::Today;
        app.consumption_selected = 0;

        // Press 'l' or Right arrow switches window forward to Last24h
        app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert_eq!(app.ledger_window, LedgerWindow::Last24h);

        // Press 'h' or Left arrow switches window backward to Today
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(app.ledger_window, LedgerWindow::Today);

        // Press 'j' (Down) moves down without changing the window!
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.ledger_window, LedgerWindow::Today);

        // Press 'k' (Up) moves back to active window summary
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.consumption_selected, 0);
        assert_eq!(app.ledger_window, LedgerWindow::Today);

        // Press Enter on window opens modal directly
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match app.modal {
            DetailModal::ConsumptionWindow { window, .. } => {
                assert_eq!(window, LedgerWindow::Today);
            }
            _ => panic!("expected ConsumptionWindow modal"),
        }
    }

    #[test]
    fn consumption_window_modal_shows_top_models_and_refresh_indicator() {
        let mut app = App::new(true, false);
        app.view = View::Consumption;
        let report = mock_consumption_report();
        app.set_consumption(report);

        app.ledger_window = LedgerWindow::Today;
        app.consumption_selected = 0;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let m_lines = modal_lines(&app.modal, 100);
        let m_text = m_lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(m_text.contains("CLIENT / HARNESS & SUB-MODEL HIERARCHY"));
        assert!(m_text.contains("▼ "));
        assert!(m_text.contains("└─ "));
        assert!(m_text.contains("TOP MODELS IN THIS WINDOW"));
        assert!(m_text.contains("PARENT HARNESS"));

        // Test modal scrolling with j / k
        assert_eq!(app.modal_scroll, 0);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.modal_scroll, 1);
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.modal_scroll, 2);
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.modal_scroll, 1);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.modal_scroll, 0);

        // Close modal
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.modal, DetailModal::None);
        assert_eq!(app.modal_scroll, 0);

        // Verify main ledger view has HARNESS column in TOP MODEL MIX
        let c_lines = consumption_lines(&app, 120);
        let c_text = c_lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(c_text.contains("HARNESS"), "Main screen must have HARNESS column");

        // Refresh indicator test
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(app.usage_busy);
        assert!(!app.limits_busy);
        for w in [80u16, 100, 120] {
            let h_lines = header_lines(&app, w, 60);
            assert!(h_lines[0].to_string().contains("refreshing..."), "width {w} first row must contain refreshing...");
            assert!(h_lines[1].to_string().contains("refreshing..."), "width {w} subtitle must contain refreshing...");
        }

        // Refresh indicator test for limits view
        let mut limits_app = App::new(true, false);
        limits_app.view = View::Limits;
        limits_app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(limits_app.limits_busy);
        assert!(!limits_app.usage_busy);
        for w in [80u16, 100, 120] {
            let h_lines = header_lines(&limits_app, w, 60);
            assert!(h_lines[0].to_string().contains("refreshing..."), "width {w} limits first row must contain refreshing...");
            assert!(h_lines[1].to_string().contains("refreshing..."), "width {w} limits subtitle must contain refreshing...");
        }
    }

    #[test]
    fn account_output_is_masked_unless_explicitly_requested() {
        let provider = ProviderSnapshot {
            provider_id: "cursor".into(),
            account_key: "private-account-id".into(),
            account_label: "person@example.com".into(),
            plan: "Free".into(),
            source: "fixture".into(),
            collected_at_ms: 0,
            source_health: SourceHealth::Connected,
            availability: Availability::Available,
            windows: vec![],
            diagnostics: vec![],
            hue: 75,
        };
        let masked = provider_identity(&provider, false).unwrap();
        assert_eq!(masked, "p***@example.com");
        assert_eq!(
            provider_identity(&provider, true).unwrap(),
            "person@example.com"
        );
        assert_eq!(provider_title_base(&provider), "Cursor · Free");
        let duplicate = provider_titles(&[&provider, &provider], false);
        assert_eq!(duplicate[0], "Cursor · Free · p***@example.com");
        let value = public_snapshot(&provider, false);
        assert_eq!(value["accountKey"], "redacted");
        assert_eq!(value["accountLabel"], "p***@example.com");
    }

    #[test]
    fn args_default_to_live_mode() {
        let mut args = Args::parse_from(["token-monitor"]);
        args.resolve_live();
        assert!(args.live);
        assert!(!args.mock);

        let mut args_mock = Args::parse_from(["token-monitor", "--mock"]);
        args_mock.resolve_live();
        assert!(!args_mock.live);
        assert!(args_mock.mock);

        let mut args_no_live = Args::parse_from(["token-monitor", "--no-live"]);
        args_no_live.resolve_live();
        assert!(!args_no_live.live);
        assert!(args_no_live.mock);

        let mut args_live = Args::parse_from(["token-monitor", "--live"]);
        args_live.resolve_live();
        assert!(args_live.live);
        assert!(!args_live.mock);
    }

    #[test]
    fn mock_consumption_ledger_is_populated_and_shows_windows() {
        let mut app = App::new(true, false);
        app.view = View::Consumption;
        let lines = body_lines(&app, 160, 40);
        let text = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Codex"));
        assert!(text.contains("Claude"));
        assert!(text.contains("Antigravity"));
        assert!(text.contains("BURN VELOCITY"));
    }

    #[test]
    fn fullscreen_limits_shows_full_labels_and_velocity_widget() {
        let app = App::new(true, false);
        let lines = body_lines(&app, 180, 40);
        let text = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Gemini Pool (Flash/Pro)"));
        assert!(text.contains("Claude/GPT Pool (Sonnet)"));
        assert!(text.contains("Modal · workspace-a"));
        assert!(text.contains("TOKEN CONSUMPTION & VELOCITY"));
        assert!(!text.contains("Modal · Modal"));
    }

    #[test]
    fn settings_screen_scrolls_viewport_when_scrolling_down() {
        let mut app = App::new(true, false);
        app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        assert_eq!(app.view, View::Settings);
        // Scroll down 15 times to select an item past the initial viewport
        for _ in 0..15 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let sel_id = &app.settings.provider_ids[app.settings.selected];
        let sel_label = token_monitor_core::provider_registry::display_name(sel_id);

        let backend = ratatui::backend::TestBackend::new(100, 14);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal.draw(|frame| draw(frame, &app, None, 60)).expect("draw");

        let mut rendered = String::new();
        for y in 0..14 {
            for x in 0..100 {
                rendered.push_str(terminal.backend().buffer().get(x, y).symbol());
            }
            rendered.push('\n');
        }
        assert!(rendered.contains("▶"));
        assert!(rendered.contains(sel_label));
    }
}
