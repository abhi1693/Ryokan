//! Manual import wizard (#122).
//!
//! `GET /system/import` renders the wizard: the start form when no
//! session is in the URL, the scanning / failed states while the
//! preview job runs, and the per-series preview once it is ready.
//! `POST /system/import` validates the start form and kicks off
//! `services::manual_import::start_preview`, then redirects to the
//! session URL. The override controls on each preview card post to
//! `/system/import/{session}/group/{idx}` and get the re-rendered
//! card back (HTMX) or a redirect to the page (plain form POST).
//!
//! All filesystem and AniList work lives in `services::manual_import`;
//! this module shapes the session for the templates and maps actions.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Response},
};
use axum_htmx::HxRequest;
use serde::Deserialize;

use crate::AppState;
use crate::handlers::responses::htmx_aware_redirect;
use crate::models::{config, series};
use crate::services::anilist::AnimeEntry;
use crate::services::manual_import::{
    self, ImportMode, ImportOptions, ImportReport, ImportSession, SessionStatus,
    import::{import_progress_id, start_import},
    preview::{self, FileView, GroupCounts, GroupView, ProjectionContext, SessionSummary},
    session,
    walk::WalkStats,
};
use crate::services::media;
use crate::services::recycle::human_bytes;

#[derive(Deserialize, Default)]
pub struct PageQuery {
    #[serde(default)]
    pub session: String,
}

#[derive(Deserialize, Default)]
pub struct StartForm {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub follow_symlinks: Option<String>,
    #[serde(default)]
    pub include_hidden: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct GroupActionForm {
    /// `skip` / `unskip` / `pick` / `unpick` / `research` /
    /// `toggle_file` / `select_all` / `select_none`.
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub candidate: Option<usize>,
    #[serde(default)]
    pub file: Option<usize>,
    #[serde(default)]
    pub query: String,
    /// `pick_id`: provider id (positive AniList, negative MAL).
    #[serde(default)]
    pub id: Option<i64>,
}

#[derive(Deserialize, Default)]
pub struct PickerQuery {
    #[serde(default)]
    pub q: String,
}

/// Start-form echo so a validation error keeps what was typed.
struct StartFormView {
    path: String,
    mode: String,
    follow_symlinks: bool,
    include_hidden: bool,
}

/// One provider candidate as the picker shows it.
struct CandidateView {
    #[allow(dead_code)]
    index: usize,
    id: i64,
    title: String,
    subtitle: String,
    cover_url: String,
    format: String,
    year: String,
    episodes: String,
    external_href: String,
    /// `AniList` / `MAL`.
    source_label: &'static str,
    /// The group's current pick.
    is_current: bool,
}

/// The picker's result list, swapped into the card by the
/// search-as-you-type input. `g` mirrors the fields the include reads
/// off a `GroupCard` so one partial serves both renders.
struct PickerCtx {
    idx: usize,
    query: String,
    picker_rows: Vec<CandidateView>,
    picker_error: String,
}

#[derive(Template)]
#[template(path = "partials/system/import_picker_results.html")]
struct PickerResultsPartial {
    session_id: String,
    g: PickerCtx,
}

/// Everything the group card partial needs. Built from a
/// [`manual_import::SeriesGroup`] plus its [`GroupView`] projection.
struct GroupCard {
    idx: usize,
    parsed_title: String,
    /// `Season 2` when the files carry a season past the first.
    season_label: String,
    year_label: String,
    /// The series name was read from a folder rather than filenames.
    title_from_folder: bool,
    query: String,
    file_count: usize,
    size: String,
    kind: &'static str,
    kind_label: &'static str,
    pick: Option<CandidateView>,
    /// Ranked candidates for the picker's initial list, current
    /// pick marked.
    picker_rows: Vec<CandidateView>,
    picker_error: String,
    low_confidence: bool,
    search_error: String,
    skipped: bool,
    existing_title: String,
    existing_anilist_id: i64,
    files: Vec<FileView>,
    counts: GroupCounts,
    /// Another group in this preview picked the same series.
    duplicate_of: String,
    /// Inline error from the last override action on this card.
    action_error: String,
}

/// A live session on the start page's "Recent scans" list.
struct RecentView {
    id: String,
    root: String,
    /// `Scanning` / `Ready to review` / `Importing` / `Imported` / `Failed`.
    status: &'static str,
}

fn recent_status(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Scanning => "Scanning",
        SessionStatus::Ready => "Ready to review",
        SessionStatus::Importing => "Importing",
        SessionStatus::Done(_) => "Imported",
        SessionStatus::Failed(_) => "Failed",
    }
}

/// A file with no series hint at all, listed under its own heading.
struct UnmatchedView {
    rel_path: String,
    episode_label: String,
    quality_label: String,
    size: String,
}

#[derive(Template)]
#[template(path = "partials/system/import_group.html")]
struct GroupPartial {
    session_id: String,
    g: GroupCard,
}

#[derive(Template)]
#[template(path = "import.html")]
struct ImportTemplate {
    page: String,
    /// Active sidebar entry for the shared System sidebar partial.
    tab: &'static str,
    title_language: String,
    media_root: String,
    /// `start` / `scanning` / `ready` / `importing` / `done` / `failed`.
    step: &'static str,
    /// Start-form validation error.
    error: String,
    /// Inline notice on the preview (e.g. a refused confirm).
    notice: String,
    /// Progress id the page watches while scanning or importing.
    progress_id: String,
    report: ImportReport,
    report_size: String,
    form: StartFormView,
    session_id: String,
    root: String,
    mode_label: &'static str,
    follow_symlinks: bool,
    include_hidden: bool,
    stats: WalkStats,
    total_size: String,
    summary: SessionSummary,
    write_size: String,
    /// Hardlink mode across filesystems: the import would copy.
    cross_fs_warning: bool,
    failed_message: String,
    groups: Vec<GroupCard>,
    unmatched: Vec<UnmatchedView>,
    /// Start page only: live sessions, newest first.
    recent: Vec<RecentView>,
    /// Always false on the page; the summary / confirm partials set
    /// it when rendered standalone for an out-of-band swap.
    oob: bool,
}

/// Summary strip, re-rendered out-of-band after every card action.
#[derive(Template)]
#[template(path = "partials/system/import_summary.html")]
struct SummaryStripPartial {
    session_id: String,
    root: String,
    mode_label: &'static str,
    stats: WalkStats,
    total_size: String,
    summary: SessionSummary,
    oob: bool,
}

/// Sticky confirm bar, re-rendered out-of-band after every card action.
#[derive(Template)]
#[template(path = "partials/system/import_confirm.html")]
struct ConfirmBarPartial {
    session_id: String,
    mode_label: &'static str,
    summary: SessionSummary,
    write_size: String,
    oob: bool,
}

/// The two out-of-band blocks for a Ready session, from a fresh
/// projection of every group (cheap: pure, no I/O beyond the render
/// context the caller already built).
fn oob_totals(session: &ImportSession, ctx: &RenderContext) -> String {
    let (_, views) = build_cards(session, ctx);
    let summary = preview::summarize(session, &views);
    let write_size = human_bytes(summary.write_bytes);
    let strip = SummaryStripPartial {
        session_id: session.id.clone(),
        root: session.root.display().to_string(),
        mode_label: session.mode.label(),
        stats: session.stats.clone(),
        total_size: human_bytes(session.stats.total_bytes),
        summary: summary.clone(),
        oob: true,
    };
    let bar = ConfirmBarPartial {
        session_id: session.id.clone(),
        mode_label: session.mode.label(),
        summary,
        write_size,
        oob: true,
    };
    format!(
        "{}{}",
        strip.render().unwrap_or_default(),
        bar.render().unwrap_or_default()
    )
}

fn format_display(format: &str) -> String {
    match format.to_ascii_uppercase().as_str() {
        "" => "TBA".to_string(),
        "TV_SHORT" => "TV Short".to_string(),
        "MOVIE" => "Movie".to_string(),
        "SPECIAL" => "Special".to_string(),
        "MUSIC" => "Music".to_string(),
        other => other.to_string(),
    }
}

fn candidate_view(index: usize, entry: &AnimeEntry, pref: &str) -> CandidateView {
    let title = preview::entry_title(entry, pref).to_string();
    let subtitle = [
        &entry.title_english,
        &entry.title_romaji,
        &entry.title_native,
    ]
    .into_iter()
    .find(|s| !s.is_empty() && **s != title)
    .cloned()
    .unwrap_or_default();
    let external_href = if entry.id > 0 {
        format!("https://anilist.co/anime/{}", entry.id)
    } else if let Some(mal) = entry.id_mal {
        format!("https://myanimelist.net/anime/{mal}")
    } else {
        String::new()
    };
    CandidateView {
        index,
        id: entry.id,
        title,
        subtitle,
        cover_url: entry.cover_url.clone(),
        format: format_display(&entry.format),
        year: entry.season_year.map(|y| y.to_string()).unwrap_or_default(),
        episodes: match entry.episodes {
            Some(n) => format!("{n} ep"),
            None => "? ep".to_string(),
        },
        external_href,
        source_label: if entry.source == "mal" {
            "MAL"
        } else {
            "AniList"
        },
        is_current: false,
    }
}

fn picker_rows(group: &manual_import::SeriesGroup, pref: &str) -> Vec<CandidateView> {
    group
        .candidates
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut v = candidate_view(i, e, pref);
            v.is_current = group.pick == Some(i);
            v
        })
        .collect()
}

/// Library state the projection reads, gathered once per render.
struct RenderContext {
    media_root: String,
    title_pref: String,
    owned_folders: HashSet<String>,
    disk_folders: HashSet<String>,
}

async fn render_context(state: &AppState) -> RenderContext {
    let cfg = config::get_config(&state.db).await.ok().flatten();
    let media_root = cfg
        .as_ref()
        .map(|c| c.media_root.trim().to_string())
        .unwrap_or_default();
    let title_pref = cfg
        .map(|c| c.title_language)
        .unwrap_or_else(|| "english".to_string());
    let owned_folders: HashSet<String> = series::get_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.folder_name)
        .filter(|f| !f.is_empty())
        .collect();
    let disk_folders: HashSet<String> =
        media::list_media_folders(&media_root).into_iter().collect();
    RenderContext {
        media_root,
        title_pref,
        owned_folders,
        disk_folders,
    }
}

impl RenderContext {
    fn projection(&self) -> ProjectionContext<'_> {
        ProjectionContext {
            media_root: &self.media_root,
            owned_folders: &self.owned_folders,
            disk_folders: &self.disk_folders,
            title_pref: &self.title_pref,
        }
    }
}

fn build_card(
    idx: usize,
    group: &manual_import::SeriesGroup,
    view: GroupView,
    pref: &str,
    duplicate_of: String,
    action_error: String,
) -> GroupCard {
    GroupCard {
        idx,
        parsed_title: group.parsed_title.clone(),
        season_label: group
            .season
            .filter(|s| *s > 1)
            .map(|s| format!("Season {s}"))
            .unwrap_or_default(),
        year_label: group.year.map(|y| y.to_string()).unwrap_or_default(),
        title_from_folder: group
            .files
            .iter()
            .any(|f| f.title_source == manual_import::parse::TitleSource::ParentFolder),
        query: group.query.clone(),
        file_count: group.files.len(),
        size: human_bytes(group.total_bytes()),
        kind: view.kind.as_str(),
        kind_label: view.kind.label(),
        pick: group
            .pick
            .and_then(|i| group.candidates.get(i).map(|e| candidate_view(i, e, pref))),
        picker_rows: picker_rows(group, pref),
        picker_error: String::new(),
        low_confidence: group.low_confidence,
        search_error: group.search_error.clone().unwrap_or_default(),
        skipped: group.skipped,
        existing_title: group
            .existing
            .as_ref()
            .map(|e| e.title.clone())
            .unwrap_or_default(),
        existing_anilist_id: group.existing.as_ref().map(|e| e.anilist_id).unwrap_or(0),
        files: view.files,
        counts: view.counts,
        duplicate_of,
        action_error,
    }
}

/// `picked AL id -> parsed title of the first non-skipped group that
/// picked it`, for the "also matched by" flag.
fn duplicate_picks(session: &ImportSession) -> HashMap<i64, String> {
    let mut first: HashMap<i64, String> = HashMap::new();
    for g in &session.groups {
        if g.skipped {
            continue;
        }
        if let Some(e) = g.picked() {
            first.entry(e.id).or_insert_with(|| g.parsed_title.clone());
        }
    }
    first
}

fn duplicate_for(firsts: &HashMap<i64, String>, group: &manual_import::SeriesGroup) -> String {
    if group.skipped {
        return String::new();
    }
    group
        .picked()
        .and_then(|e| firsts.get(&e.id))
        .filter(|t| **t != group.parsed_title)
        .cloned()
        .unwrap_or_default()
}

fn build_cards(session: &ImportSession, ctx: &RenderContext) -> (Vec<GroupCard>, Vec<GroupView>) {
    let proj = ctx.projection();
    let firsts = duplicate_picks(session);
    let views: Vec<GroupView> = session
        .groups
        .iter()
        .map(|g| preview::project_group(g, &proj))
        .collect();
    let cards = session
        .groups
        .iter()
        .zip(views.iter())
        .enumerate()
        .map(|(idx, (g, v))| {
            build_card(
                idx,
                g,
                v.clone(),
                &ctx.title_pref,
                duplicate_for(&firsts, g),
                String::new(),
            )
        })
        .collect();
    (cards, views)
}

fn empty_form() -> StartFormView {
    StartFormView {
        path: String::new(),
        mode: "hardlink".to_string(),
        follow_symlinks: false,
        include_hidden: false,
    }
}

fn base_template(ctx: &RenderContext) -> ImportTemplate {
    ImportTemplate {
        // Reached from System → Import Library; highlight that nav entry.
        page: "system".to_string(),
        tab: "import",
        title_language: ctx.title_pref.clone(),
        media_root: ctx.media_root.clone(),
        step: "start",
        error: String::new(),
        notice: String::new(),
        progress_id: String::new(),
        report: ImportReport::default(),
        report_size: String::new(),
        form: empty_form(),
        session_id: String::new(),
        root: String::new(),
        mode_label: ImportMode::Hardlink.label(),
        follow_symlinks: false,
        include_hidden: false,
        stats: WalkStats::default(),
        total_size: String::new(),
        summary: SessionSummary::default(),
        write_size: String::new(),
        cross_fs_warning: false,
        failed_message: String::new(),
        groups: Vec::new(),
        unmatched: Vec::new(),
        recent: Vec::new(),
        oob: false,
    }
}

fn recent_sessions(state: &AppState) -> Vec<RecentView> {
    session::list(&state.import_sessions)
        .into_iter()
        .map(|s| RecentView {
            id: s.id.clone(),
            root: s.root.display().to_string(),
            status: recent_status(&s.status),
        })
        .collect()
}

fn render(t: ImportTemplate) -> Html<String> {
    Html(t.render().unwrap_or_default())
}

fn session_url(id: &str) -> String {
    format!("/system/import?session={id}")
}

pub async fn page(State(state): State<AppState>, Query(q): Query<PageQuery>) -> Html<String> {
    render_session_page(&state, q.session.trim(), String::new()).await
}

/// The wizard page for `session_id` in whatever state it is in.
/// `notice` renders above the preview (confirm refusals).
async fn render_session_page(state: &AppState, id: &str, notice: String) -> Html<String> {
    let ctx = render_context(state).await;
    let mut t = base_template(&ctx);
    if id.is_empty() {
        t.recent = recent_sessions(state);
        return render(t);
    }
    let session = if session::is_valid_id(id) {
        session::get(&state.import_sessions, id)
    } else {
        None
    };
    let Some(session) = session else {
        t.error = "That preview has expired. Start a new scan.".to_string();
        t.recent = recent_sessions(state);
        return render(t);
    };

    t.session_id = session.id.clone();
    t.root = session.root.display().to_string();
    t.mode_label = session.mode.label();
    t.follow_symlinks = session.follow_symlinks;
    t.include_hidden = session.include_hidden;
    t.notice = notice;
    match &session.status {
        SessionStatus::Scanning => {
            t.step = "scanning";
            t.progress_id = session.id.clone();
        }
        SessionStatus::Importing => {
            t.step = "importing";
            t.progress_id = import_progress_id(&session.id);
        }
        SessionStatus::Done(report) => {
            t.step = "done";
            t.report_size = human_bytes(report.bytes_written);
            t.report = (**report).clone();
        }
        SessionStatus::Failed(msg) => {
            t.step = "failed";
            t.failed_message = msg.clone();
        }
        SessionStatus::Ready => {
            t.step = "ready";
            let (cards, views) = build_cards(&session, &ctx);
            let summary = preview::summarize(&session, &views);
            t.write_size = human_bytes(summary.write_bytes);
            t.summary = summary;
            t.groups = cards;
            t.stats = session.stats.clone();
            t.total_size = human_bytes(session.stats.total_bytes);
            t.cross_fs_warning =
                session.mode == ImportMode::Hardlink && session.cross_fs == Some(true);
            t.unmatched = session
                .unmatched_files
                .iter()
                .map(|f| UnmatchedView {
                    rel_path: f.rel_path.clone(),
                    episode_label: preview::episode_label(f.episode),
                    quality_label: f.quality_label.clone(),
                    size: human_bytes(f.size_bytes),
                })
                .collect();
        }
    }
    render(t)
}

fn checkbox_on(v: &Option<String>) -> bool {
    v.as_deref()
        .is_some_and(|s| matches!(s, "1" | "on" | "true"))
}

/// Start-form validation. Returns the session to start or the message
/// to show. Path checks are a couple of `stat`s; fine on the request
/// path.
fn validate_start(form: &StartForm, media_root: &str) -> Result<ImportSession, String> {
    let path = form.path.trim();
    if media_root.is_empty() {
        return Err(
            "Set a media root under Settings before importing. Imported files are placed under it."
                .to_string(),
        );
    }
    if path.is_empty() {
        return Err("Enter the folder to scan.".to_string());
    }
    let root = PathBuf::from(path);
    if !root.is_absolute() {
        return Err("Enter an absolute path, for example /mnt/anime.".to_string());
    }
    if !root.is_dir() {
        return Err(format!(
            "{} is not a folder Ryokan can read. Check the path, and if Ryokan runs in Docker make sure the folder is mounted into the container.",
            root.display()
        ));
    }
    let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let canonical_media =
        std::fs::canonicalize(media_root).unwrap_or_else(|_| PathBuf::from(media_root));
    if canonical_root.starts_with(&canonical_media) {
        return Err(format!(
            "{} is inside your Ryokan media root ({}). Ryokan already tracks files there; point the wizard at a folder outside it.",
            root.display(),
            media_root
        ));
    }
    let mode = if form.mode.trim().is_empty() {
        ImportMode::Hardlink
    } else {
        ImportMode::parse(&form.mode).ok_or_else(|| "Unknown import mode.".to_string())?
    };
    Ok(ImportSession::new(
        session::mint_id(),
        root,
        mode,
        checkbox_on(&form.follow_symlinks),
        checkbox_on(&form.include_hidden),
    ))
}

pub async fn start(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Form(form): Form<StartForm>,
) -> Response {
    let ctx = render_context(&state).await;
    match validate_start(&form, &ctx.media_root) {
        Ok(session) => {
            let id = session.id.clone();
            manual_import::start_preview(&state, session).await;
            htmx_aware_redirect(is_htmx, &session_url(&id))
        }
        Err(msg) => {
            let mut t = base_template(&ctx);
            t.error = msg;
            t.recent = recent_sessions(&state);
            t.form = StartFormView {
                path: form.path.trim().to_string(),
                mode: if ImportMode::parse(&form.mode).is_some() {
                    form.mode.trim().to_ascii_lowercase()
                } else {
                    "hardlink".to_string()
                },
                follow_symlinks: checkbox_on(&form.follow_symlinks),
                include_hidden: checkbox_on(&form.include_hidden),
            };
            render(t).into_response()
        }
    }
}

/// Apply one override action. Errors are user-facing strings the card
/// shows inline.
async fn apply_group_action(
    state: &AppState,
    session_id: &str,
    idx: usize,
    form: &GroupActionForm,
) -> Result<(), String> {
    let set_group = |f: &dyn Fn(&mut manual_import::SeriesGroup)| -> Result<(), String> {
        session::update(&state.import_sessions, session_id, |s| {
            match s.groups.get_mut(idx) {
                Some(g) => {
                    f(g);
                    Ok(())
                }
                None => Err("Unknown series in this preview".to_string()),
            }
        })
        .unwrap_or_else(|| Err("Preview session not found".to_string()))
    };
    match form.action.as_str() {
        "skip" => set_group(&|g| g.skipped = true),
        "unskip" => set_group(&|g| g.skipped = false),
        "pick" => {
            let Some(candidate) = form.candidate else {
                return Err("Pick a series".to_string());
            };
            manual_import::pick_candidate(state, session_id, idx, Some(candidate)).await
        }
        "unpick" => manual_import::pick_candidate(state, session_id, idx, None).await,
        "pick_id" => {
            let Some(id) = form.id else {
                return Err("Pick a series".to_string());
            };
            manual_import::pick_by_id(state, session_id, idx, id).await
        }
        "research" => manual_import::research_group(state, session_id, idx, &form.query).await,
        "toggle_file" => {
            let Some(file) = form.file else {
                return Err("Unknown file".to_string());
            };
            set_group(&|g| {
                if let Some(f) = g.files.get_mut(file) {
                    f.selected = !f.selected;
                }
            })
        }
        "select_all" => set_group(&|g| g.files.iter_mut().for_each(|f| f.selected = true)),
        "select_none" => set_group(&|g| g.files.iter_mut().for_each(|f| f.selected = false)),
        other => Err(format!("Unknown action {other}")),
    }
}

pub async fn group_action(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path((session_id, idx)): Path<(String, usize)>,
    Form(form): Form<GroupActionForm>,
) -> Response {
    if !session::is_valid_id(&session_id)
        || session::get(&state.import_sessions, &session_id).is_none()
    {
        // Expired or foreign id: the page explains and offers a fresh
        // start. HX-Redirect for the HTMX caller, 303 otherwise.
        return htmx_aware_redirect(is_htmx, &session_url(&session_id));
    }
    let action_error = apply_group_action(&state, &session_id, idx, &form)
        .await
        .err()
        .unwrap_or_default();

    if !is_htmx {
        return htmx_aware_redirect(
            false,
            &format!("{}#import-group-{}", session_url(&session_id), idx),
        );
    }
    let Some(session) = session::get(&state.import_sessions, &session_id) else {
        return htmx_aware_redirect(true, &session_url(&session_id));
    };
    let Some(group) = session.groups.get(idx) else {
        return htmx_aware_redirect(true, &session_url(&session_id));
    };
    let ctx = render_context(&state).await;
    let view = preview::project_group(group, &ctx.projection());
    let firsts = duplicate_picks(&session);
    let card = build_card(
        idx,
        group,
        view,
        &ctx.title_pref,
        duplicate_for(&firsts, group),
        action_error,
    );
    let partial = GroupPartial {
        session_id: session_id.clone(),
        g: card,
    };
    // The card, then the summary strip and confirm bar out-of-band so
    // the totals follow every skip / tick / pick.
    let html = format!(
        "{}{}",
        partial.render().unwrap_or_default(),
        oob_totals(&session, &ctx)
    );
    Html(html).into_response()
}

/// The picker's result list: the ranked candidates when `q` is empty,
/// a live provider search otherwise. Always 200 so the swap lands;
/// failures render inline.
pub async fn picker_candidates(
    State(state): State<AppState>,
    Path((session_id, idx)): Path<(String, usize)>,
    Query(q): Query<PickerQuery>,
) -> Html<String> {
    let query = q.q.trim().to_string();
    let ctx = render_context(&state).await;
    let group = if session::is_valid_id(&session_id) {
        session::get(&state.import_sessions, &session_id).and_then(|s| s.groups.get(idx).cloned())
    } else {
        None
    };
    let Some(group) = group else {
        let partial = PickerResultsPartial {
            session_id,
            g: PickerCtx {
                idx,
                query,
                picker_rows: Vec::new(),
                picker_error: "This preview has expired. Start a new scan.".to_string(),
            },
        };
        return Html(partial.render().unwrap_or_default());
    };
    let (rows, error) = if query.is_empty() {
        (picker_rows(&group, &ctx.title_pref), String::new())
    } else {
        match manual_import::live_search(&state, &session_id, idx, &query).await {
            Ok(hits) => {
                let current = group.picked().map(|e| e.id);
                (
                    hits.iter()
                        .enumerate()
                        .map(|(i, e)| {
                            let mut v = candidate_view(i, e, &ctx.title_pref);
                            v.is_current = current == Some(e.id);
                            v
                        })
                        .collect(),
                    String::new(),
                )
            }
            Err(e) => (Vec::new(), format!("Search failed: {e}")),
        }
    };
    let partial = PickerResultsPartial {
        session_id,
        g: PickerCtx {
            idx,
            query,
            picker_rows: rows,
            picker_error: error,
        },
    };
    Html(partial.render().unwrap_or_default())
}

/// Confirm: start the import job for a Ready preview with something
/// to write. Refusals re-render the preview with the reason instead of
/// redirecting, so the user keeps their corrections on screen.
pub async fn confirm(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path(session_id): Path<String>,
) -> Response {
    if !session::is_valid_id(&session_id) {
        return htmx_aware_redirect(is_htmx, &session_url(&session_id));
    }
    let Some(session) = session::get(&state.import_sessions, &session_id) else {
        return htmx_aware_redirect(is_htmx, &session_url(&session_id));
    };
    if session.status != SessionStatus::Ready {
        return htmx_aware_redirect(is_htmx, &session_url(&session_id));
    }
    let ctx = render_context(&state).await;
    if ctx.media_root.is_empty() {
        return render_session_page(
            &state,
            &session_id,
            "Set a media root under Settings before importing.".to_string(),
        )
        .await
        .into_response();
    }
    let writes = manual_import::import::writes_for(&session, &ctx.projection());
    if writes == 0 {
        return render_session_page(
            &state,
            &session_id,
            "Nothing to import: every file is excluded, already present, or unmatched.".to_string(),
        )
        .await
        .into_response();
    }
    match start_import(&state, &session_id, ImportOptions::default()).await {
        Ok(()) => htmx_aware_redirect(is_htmx, &session_url(&session_id)),
        Err(msg) => render_session_page(&state, &session_id, msg)
            .await
            .into_response(),
    }
}

/// Cancel a running import. The job stops between files; the page
/// shows the partial report when it lands.
pub async fn cancel(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path(session_id): Path<String>,
) -> Response {
    if session::is_valid_id(&session_id) {
        session::update(&state.import_sessions, &session_id, |s| {
            if s.status == SessionStatus::Importing {
                s.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
    htmx_aware_redirect(is_htmx, &session_url(&session_id))
}

pub async fn discard(
    State(state): State<AppState>,
    HxRequest(is_htmx): HxRequest,
    Path(session_id): Path<String>,
) -> Response {
    if session::is_valid_id(&session_id) {
        session::remove(&state.import_sessions, &session_id);
    }
    htmx_aware_redirect(is_htmx, "/system/import")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(path: &str, mode: &str) -> StartForm {
        StartForm {
            path: path.to_string(),
            mode: mode.to_string(),
            follow_symlinks: Some("1".into()),
            include_hidden: None,
        }
    }

    #[test]
    fn validate_requires_media_root_then_path() {
        let err = validate_start(&form("/tmp", "hardlink"), "").unwrap_err();
        assert!(err.contains("media root"), "{err}");
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let err = validate_start(&form("  ", "hardlink"), media.to_str().unwrap()).unwrap_err();
        assert!(err.contains("Enter the folder"), "{err}");
        let err = validate_start(&form("relative/path", "hardlink"), media.to_str().unwrap())
            .unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn validate_rejects_missing_dir_and_media_root_subtree() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        let inside = media.join("Show");
        std::fs::create_dir_all(&inside).unwrap();
        let media_s = media.to_str().unwrap();

        let missing = tmp.path().join("missing");
        let err = validate_start(&form(missing.to_str().unwrap(), "copy"), media_s).unwrap_err();
        assert!(err.contains("not a folder"), "{err}");

        let err = validate_start(&form(inside.to_str().unwrap(), "copy"), media_s).unwrap_err();
        assert!(err.contains("inside your Ryokan media root"), "{err}");
        let err = validate_start(&form(media_s, "copy"), media_s).unwrap_err();
        assert!(err.contains("inside your Ryokan media root"), "{err}");
    }

    #[test]
    fn validate_builds_session_with_mode_and_toggles() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&media).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        let s = validate_start(
            &form(src.to_str().unwrap(), "move"),
            media.to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(s.mode, ImportMode::Move);
        assert!(s.follow_symlinks);
        assert!(!s.include_hidden);
        assert!(session::is_valid_id(&s.id));
        assert_eq!(s.status, SessionStatus::Scanning);

        // Empty mode defaults to hardlink; garbage is rejected.
        let s = validate_start(&form(src.to_str().unwrap(), ""), media.to_str().unwrap()).unwrap();
        assert_eq!(s.mode, ImportMode::Hardlink);
        let err = validate_start(
            &form(src.to_str().unwrap(), "symlink"),
            media.to_str().unwrap(),
        )
        .unwrap_err();
        assert!(err.contains("Unknown import mode"), "{err}");
    }

    #[test]
    fn format_display_maps_al_enums() {
        assert_eq!(format_display("TV"), "TV");
        assert_eq!(format_display("TV_SHORT"), "TV Short");
        assert_eq!(format_display("MOVIE"), "Movie");
        assert_eq!(format_display(""), "TBA");
    }
}

/// Handler-level coverage through a hand-built router (the shared
/// `handler_router` only mounts `/api/health`). No auth layer: these
/// exercise the handlers, not `require_auth`. The one background job
/// test uses a fixture with no series hint so the pipeline finishes
/// without touching AniList.
#[cfg(test)]
mod router_tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::routing::{get, post};
    use tower::ServiceExt;

    use super::*;
    use crate::models::config::{Config, save_config};
    use crate::services::anilist::AnimeEntry;
    use crate::services::manual_import::parse::TitleSource;
    use crate::services::manual_import::{CandidateFile, ExistingSeries, SeriesGroup};
    use crate::test_support::{build_test_app_state, in_memory_pool, seed_series};

    fn router(state: AppState) -> Router {
        Router::new()
            .route("/system/import", get(page).post(start))
            .route(
                "/system/import/{session_id}/group/{idx}",
                post(group_action),
            )
            .route(
                "/system/import/{session_id}/group/{idx}/candidates",
                get(picker_candidates),
            )
            .route("/system/import/{session_id}/discard", post(discard))
            .with_state(state)
    }

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn get_page(app: &Router, uri: &str) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        (status, body_text(resp).await)
    }

    async fn post_form(
        app: &Router,
        uri: &str,
        form: &str,
        htmx: bool,
    ) -> axum::response::Response {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if htmx {
            req = req.header("HX-Request", "true");
        }
        app.clone()
            .oneshot(req.body(Body::from(form.to_string())).unwrap())
            .await
            .unwrap()
    }

    async fn seed_media_root(db: &sqlx::SqlitePool, media_root: &str) {
        let cfg = Config {
            media_root: media_root.to_string(),
            ..Config::default()
        };
        save_config(db, &cfg).await.unwrap();
    }

    fn entry(id: i64, english: &str) -> AnimeEntry {
        AnimeEntry {
            id,
            id_mal: None,
            title_romaji: format!("{english} Romaji"),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: String::new(),
            episodes: Some(12),
            season_year: Some(2020),
            source: "anilist".into(),
            average_score: None,
        }
    }

    fn file(name: &str, episode: Option<i32>) -> CandidateFile {
        CandidateFile {
            path: PathBuf::from(format!("/src/Show/{name}")),
            rel_path: format!("Show/{name}"),
            file_name: name.to_string(),
            size_bytes: 1024,
            parsed_title: Some("Show".into()),
            title_source: TitleSource::Filename,
            season: None,
            episode,
            year: None,
            group: None,
            quality_label: "WEB-1080p".into(),
            selected: true,
            source_episode: None,
        }
    }

    /// A Ready session with one matched group and two candidates.
    fn ready_session(state: &AppState) -> String {
        let mut s = ImportSession::new(
            session::mint_id(),
            PathBuf::from("/src"),
            ImportMode::Hardlink,
            false,
            false,
        );
        s.status = SessionStatus::Ready;
        s.stats.files = 2;
        s.groups.push(SeriesGroup {
            key: "show".into(),
            parsed_title: "Show".into(),
            season: None,
            tmdb_season: None,
            year: None,
            query: "Show".into(),
            files: vec![
                file("Show - 01.mkv", Some(1)),
                file("Show - 02.mkv", Some(2)),
            ],
            candidates: vec![entry(100, "Show"), entry(101, "Show Alternative")],
            pick: Some(0),
            low_confidence: false,
            search_error: None,
            skipped: false,
            existing: None,
            mapping_note: None,
            search_results: Vec::new(),
        });
        let id = s.id.clone();
        session::insert(&state.import_sessions, s);
        id
    }

    #[tokio::test]
    async fn start_form_renders_and_warns_without_media_root() {
        let db = in_memory_pool().await;
        let app = router(build_test_app_state(db, None));
        let (status, body) = get_page(&app, "/system/import").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Import an existing library"));
        assert!(body.contains("Scan folder"));
        // Rendered inside the System shell with the sidebar entry active.
        assert!(body.contains("tabbed-sidebar"), "{body}");
        assert!(
            body.contains("href=\"/system/import\" class=\"tabbed-side-tab active\""),
            "{body}"
        );
        assert!(
            body.contains("No media root set"),
            "warns when media root is unset"
        );
        assert!(
            body.contains("disabled"),
            "scan button disabled without a media root"
        );
    }

    #[tokio::test]
    async fn start_page_lists_live_sessions() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        let state_ref = state.clone();
        let app = router(state);
        let (_, body) = get_page(&app, "/system/import").await;
        assert!(body.contains("Recent scans"), "{body}");
        assert!(
            body.contains(&format!("/system/import?session={id}")),
            "{body}"
        );
        // A ready scan is listed without a status label; only a
        // running / failed one says so.
        assert!(!body.contains("Ready to review"), "{body}");
        session::update(&state_ref.import_sessions, &id, |s| {
            s.status = SessionStatus::Importing
        });
        let (_, body) = get_page(&app, "/system/import").await;
        assert!(body.contains("import-recent-meta\">Importing"), "{body}");
    }

    #[tokio::test]
    async fn unknown_or_malformed_session_shows_expired() {
        let db = in_memory_pool().await;
        let app = router(build_test_app_state(db, None));
        let (status, body) = get_page(&app, "/system/import?session=nope").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("That preview has expired"));
        let fresh = session::mint_id();
        let (_, body) = get_page(&app, &format!("/system/import?session={fresh}")).await;
        assert!(body.contains("That preview has expired"));
    }

    #[tokio::test]
    async fn start_with_bad_path_rerenders_form_with_error_and_echo() {
        let db = in_memory_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        seed_media_root(&db, media.to_str().unwrap()).await;
        let app = router(build_test_app_state(db, None));

        let resp = post_form(
            &app,
            "/system/import",
            "path=%2Fdefinitely%2Fmissing%2Fdir&mode=copy&include_hidden=1",
            false,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("is not a folder Ryokan can read"), "{body}");
        assert!(
            body.contains("value=\"/definitely/missing/dir\""),
            "echoes the typed path"
        );
        assert!(
            body.contains("<option value=\"copy\" selected>"),
            "echoes the chosen mode"
        );
        assert!(
            body.contains("name=\"include_hidden\" value=\"1\" checked"),
            "echoes the hidden toggle"
        );
    }

    #[tokio::test]
    async fn start_runs_pipeline_and_page_reaches_ready() {
        let db = in_memory_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&media).unwrap();
        // No series hint anywhere, so the preview needs no AniList
        // call: one unmatched file plus a non-video sidecar.
        std::fs::create_dir_all(src.join("Season 01")).unwrap();
        std::fs::write(src.join("Season 01/01.mkv"), b"xx").unwrap();
        std::fs::write(src.join("Season 01/01.nfo"), b"x").unwrap();
        seed_media_root(&db, media.to_str().unwrap()).await;
        let state = build_test_app_state(db, None);
        let app = router(state.clone());

        // HTMX caller gets 200 + HX-Redirect; plain caller gets 303.
        let form = format!(
            "path={}&mode=hardlink",
            urlencoding::encode(src.to_str().unwrap())
        );
        let resp = post_form(&app, "/system/import", &form, true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let location = resp
            .headers()
            .get("HX-Redirect")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            location.starts_with("/system/import?session="),
            "{location}"
        );
        let session_id = location
            .trim_start_matches("/system/import?session=")
            .to_string();
        assert!(session::is_valid_id(&session_id));

        let resp = post_form(&app, "/system/import", &form, false).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);

        // The job runs in the background; poll the page until Ready.
        let mut body = String::new();
        for _ in 0..100 {
            let (status, b) = get_page(&app, &location).await;
            assert_eq!(status, StatusCode::OK);
            body = b;
            if !body.contains("import-scanning") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !body.contains("import-scanning"),
            "preview never left the scanning state"
        );
        assert!(body.contains("Files with no series hint"), "{body}");
        assert!(body.contains("Season 01/01.mkv"));
        assert!(body.contains(">E01<"));
        assert!(body.contains("Discard preview"));
        let s = session::get(&state.import_sessions, &session_id).unwrap();
        assert_eq!(s.status, SessionStatus::Ready);
        assert_eq!(s.stats.files, 1);
        assert_eq!(s.stats.skipped_non_video, 1);
        assert_eq!(s.unmatched_files.len(), 1);
        assert!(s.groups.is_empty());
    }

    #[tokio::test]
    async fn ready_page_renders_group_card_and_summary() {
        let db = in_memory_pool().await;
        seed_media_root(&db, "/media").await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        let app = router(state);
        let (status, body) = get_page(&app, &format!("/system/import?session={id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("import-group-new"), "new-series card");
        assert!(body.contains("Show Alternative"), "alternative offered");
        assert!(
            body.contains("Show/Season 01/Show - 01.mkv"),
            "projected destination"
        );
        assert!(
            body.contains("<strong>1</strong> new"),
            "summary counts one new series"
        );
        assert!(body.contains("Import 2 files"), "{body}");
    }

    #[tokio::test]
    async fn group_actions_swap_card_via_htmx_and_redirect_otherwise() {
        let db = in_memory_pool().await;
        seed_media_root(&db, "/media").await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        let app = router(state.clone());
        let uri = format!("/system/import/{id}/group/0");

        // Skip: card re-renders as skipped, files gone from the table,
        // and the summary strip + confirm bar come along out-of-band
        // with the new totals (nothing left to import).
        let resp = post_form(&app, &uri, "action=skip", true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("import-group-skipped"), "{body}");
        assert!(body.contains("Include"), "offers to include again");
        assert!(
            !body.contains("import-files"),
            "skipped card hides the file table"
        );
        assert!(
            body.contains("id=\"import-summary\" hx-swap-oob=\"true\""),
            "{body}"
        );
        assert!(
            body.contains("id=\"import-confirm\" hx-swap-oob=\"true\""),
            "{body}"
        );
        assert!(body.contains("<strong>1</strong> skipped"), "{body}");
        assert!(body.contains("Nothing to import"), "{body}");

        // Unskip via plain POST: 303 back to the card anchor.
        let resp = post_form(&app, &uri, "action=unskip", false).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(loc, format!("/system/import?session={id}#import-group-0"));
        assert!(!session::get(&state.import_sessions, &id).unwrap().groups[0].skipped);

        // Toggle a file off: row shows Excluded and counts drop.
        let body = body_text(post_form(&app, &uri, "action=toggle_file&file=1", true).await).await;
        assert!(body.contains("import-file-deselected"), "{body}");
        assert!(
            body.contains("<strong>1</strong> to import, 1 excluded"),
            "{body}"
        );
        let body = body_text(post_form(&app, &uri, "action=select_all", true).await).await;
        assert!(!body.contains("import-file-deselected"));
        let body = body_text(post_form(&app, &uri, "action=select_none", true).await).await;
        assert!(
            body.contains("<strong>0</strong> to import, 2 excluded"),
            "{body}"
        );

        // Pick the alternative, then none.
        let body = body_text(post_form(&app, &uri, "action=pick&candidate=1", true).await).await;
        assert!(body.contains("anilist.co/anime/101"), "{body}");
        assert_eq!(
            session::get(&state.import_sessions, &id).unwrap().groups[0].pick,
            Some(1)
        );
        let body = body_text(post_form(&app, &uri, "action=pick&candidate=9", true).await).await;
        assert!(body.contains("Unknown candidate"), "inline error: {body}");
        let body = body_text(post_form(&app, &uri, "action=unpick", true).await).await;
        assert!(body.contains("No match found"), "{body}");
        assert!(body.contains("import-group-nomatch"));

        // Empty re-search is refused inline without touching AL.
        let body = body_text(post_form(&app, &uri, "action=research&query=+", true).await).await;
        assert!(body.contains("Type a title to search for"), "{body}");
    }

    #[tokio::test]
    async fn picker_lists_ranked_candidates_and_pick_id_switches() {
        let db = in_memory_pool().await;
        seed_media_root(&db, "/media").await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        let app = router(state.clone());

        // The card embeds the picker with the current pick marked.
        let (_, body) = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("import-picker"), "{body}");
        assert!(body.contains("Change match"));
        assert!(
            body.contains("disabled aria-current=\"true\">Current<"),
            "{body}"
        );
        assert!(
            body.contains("name=\"id\" value=\"101\""),
            "alternative offered with a Use button"
        );

        // The candidates endpoint serves the same list on its own.
        let (status, body) =
            get_page(&app, &format!("/system/import/{id}/group/0/candidates")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Show Alternative"), "{body}");
        assert!(body.contains(">Use<"));
        assert!(!body.contains("tabbed-page"), "a fragment, not a page");

        // pick_id switches the card; an unknown id is refused inline.
        let uri = format!("/system/import/{id}/group/0");
        let body = body_text(post_form(&app, &uri, "action=pick_id&id=101", true).await).await;
        assert!(body.contains("anilist.co/anime/101"), "{body}");
        assert_eq!(
            session::get(&state.import_sessions, &id).unwrap().groups[0].pick,
            Some(1)
        );
        let body = body_text(post_form(&app, &uri, "action=pick_id&id=999", true).await).await;
        assert!(body.contains("Unknown candidate"), "{body}");

        // A no-match card opens the picker by itself.
        let body = body_text(post_form(&app, &uri, "action=unpick", true).await).await;
        assert!(
            body.contains("<details class=\"import-picker\" open>"),
            "{body}"
        );
        assert!(body.contains("Find a match"));
    }

    #[tokio::test]
    async fn merge_card_marks_existing_series_and_present_episode() {
        let db = in_memory_pool().await;
        seed_media_root(&db, "/media").await;
        let series_id = seed_series(&db, 100, "Show On Disk").await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        // Attach the library row the way the resolver would.
        let mut tags = HashMap::new();
        tags.insert(
            1,
            manual_import::ExistingTag {
                quality_label: "BD-1080p".into(),
                state: "completed".into(),
                manual_override: false,
                classification: crate::services::source::classify_release_sync(
                    "[G] Show - 01 [BD 1080p].mkv",
                    None,
                ),
            },
        );
        session::update(&state.import_sessions, &id, |s| {
            s.groups[0].existing = Some(ExistingSeries {
                id: series_id,
                anilist_id: 100,
                title: "Show On Disk".into(),
                folder_name: "Show On Disk".into(),
                tags,
            });
        });
        let app = router(state);
        let (_, body) = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("import-group-merge"), "{body}");
        assert!(body.contains("In your library as"));
        assert!(body.contains("/series/100"));
        assert!(
            body.contains("import-status-present"),
            "episode 1 already have"
        );
        assert!(body.contains("have BD-1080p"));
        assert!(body.contains("Show On Disk/Season 01/Show - 02.mkv"));
        assert!(body.contains("<strong>1</strong> already in library"));
    }

    #[tokio::test]
    async fn expired_session_actions_redirect_and_discard_removes() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let id = ready_session(&state);
        let app = router(state.clone());

        let ghost = session::mint_id();
        let resp = post_form(
            &app,
            &format!("/system/import/{ghost}/group/0"),
            "action=skip",
            true,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().contains_key("HX-Redirect"));

        let resp = post_form(&app, &format!("/system/import/{id}/discard"), "", false).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers()
                .get(header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "/system/import"
        );
        assert!(session::get(&state.import_sessions, &id).is_none());
    }
}

/// Confirm / cancel / report coverage. The confirm test runs the real
/// job: AniList is pointed at a closed local port so the metadata
/// hydration path executes and fails fast without the network.
#[cfg(test)]
mod import_router_tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::routing::{get, post};
    use std::sync::LazyLock;
    use tower::ServiceExt;

    use super::*;
    use crate::models::config::{Config, save_config};
    use crate::services::anilist::{self, AnimeEntry};
    use crate::services::manual_import::{CandidateFile, SeriesGroup, parse::TitleSource};
    use crate::test_support::{build_test_app_state, in_memory_pool};

    /// Serializes the env-var flip across tests in this module (nextest
    /// runs one process per test, so this only matters under plain
    /// `cargo test`).
    static ENV_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    fn router(state: AppState) -> Router {
        Router::new()
            .route("/system/import", get(page).post(start))
            .route("/system/import/{session_id}/confirm", post(confirm))
            .route("/system/import/{session_id}/cancel", post(cancel))
            .route("/system/import/{session_id}/discard", post(discard))
            .with_state(state)
    }

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn get_page(app: &Router, uri: &str) -> String {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        body_text(resp).await
    }

    async fn post_empty(app: &Router, uri: &str, htmx: bool) -> axum::response::Response {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if htmx {
            req = req.header("HX-Request", "true");
        }
        app.clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn entry(id: i64, english: &str) -> AnimeEntry {
        AnimeEntry {
            id,
            id_mal: None,
            title_romaji: format!("{english} Romaji"),
            title_english: english.to_string(),
            title_native: String::new(),
            cover_url: String::new(),
            format: "TV".into(),
            status: "FINISHED".into(),
            status_display: String::new(),
            episodes: Some(12),
            season_year: Some(2020),
            source: "anilist".into(),
            average_score: None,
        }
    }

    /// Real files under a tempdir so the job has something to link.
    fn session_with_files(state: &AppState, root: &std::path::Path, selected: bool) -> String {
        let mut files = Vec::new();
        for (i, name) in ["Show - 01.mkv", "Show - 02.mkv"].iter().enumerate() {
            let path = root.join("Show").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"xx").unwrap();
            files.push(CandidateFile {
                path,
                rel_path: format!("Show/{name}"),
                file_name: name.to_string(),
                size_bytes: 2,
                parsed_title: Some("Show".into()),
                title_source: TitleSource::Filename,
                season: None,
                episode: Some(i as i32 + 1),
                year: None,
                group: None,
                quality_label: "Unknown".into(),
                selected,
                source_episode: None,
            });
        }
        let mut s = ImportSession::new(
            session::mint_id(),
            root.to_path_buf(),
            ImportMode::Hardlink,
            false,
            false,
        );
        s.status = SessionStatus::Ready;
        s.stats.files = 2;
        s.groups.push(SeriesGroup {
            key: "show".into(),
            parsed_title: "Show".into(),
            season: None,
            tmdb_season: None,
            year: None,
            query: "Show".into(),
            files,
            candidates: vec![entry(100, "Show")],
            pick: Some(0),
            low_confidence: false,
            search_error: None,
            skipped: false,
            existing: None,
            mapping_note: None,
            search_results: Vec::new(),
        });
        let id = s.id.clone();
        session::insert(&state.import_sessions, s);
        id
    }

    async fn seed_media_root(db: &sqlx::SqlitePool, media_root: &str) {
        let cfg = Config {
            media_root: media_root.to_string(),
            ..Config::default()
        };
        save_config(db, &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn confirm_runs_the_job_and_page_shows_the_report() {
        let _gate = ENV_LOCK.lock().await;
        // Closed port for every metadata provider: the hydration path
        // runs, fails fast (AL, then the Jikan and Kitsu fallbacks),
        // and the import carries on the way it would through an
        // outage. Nothing here reaches the network.
        anilist::reset_state_for_tests();
        unsafe {
            std::env::set_var("RYOKAN_ANILIST_API_BASE", "http://127.0.0.1:9");
            std::env::set_var("JIKAN_API_BASE", "http://127.0.0.1:9");
            std::env::set_var("RYOKAN_KITSU_API_BASE", "http://127.0.0.1:9");
        }
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let db = in_memory_pool().await;
        seed_media_root(&db, media.to_str().unwrap()).await;
        let state = build_test_app_state(db, None);
        let id = session_with_files(&state, &tmp.path().join("src"), true);
        let app = router(state.clone());

        // Ready page carries the confirm bar.
        let body = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("Import 2 files"), "{body}");
        assert!(body.contains("data-ryokan-confirm-title"));

        let resp = post_empty(&app, &format!("/system/import/{id}/confirm"), true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("HX-Redirect").unwrap().to_str().unwrap(),
            format!("/system/import?session={id}")
        );

        let mut body = String::new();
        for _ in 0..1500 {
            body = get_page(&app, &format!("/system/import?session={id}")).await;
            if !body.contains("import-importing") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(body.contains("Import complete"), "{body}");
        assert!(body.contains("import-status-import\">Created"), "{body}");
        assert!(body.contains("<strong>2</strong> imported"), "{body}");
        assert!(media.join("Show/Season 01/Show - 01.mkv").exists());
        let row = series::get_by_anilist_id(&state.db, 100)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.folder_name, "Show");

        // A second confirm on the finished session is refused (not
        // Ready) and just routes back to the page.
        let resp = post_empty(&app, &format!("/system/import/{id}/confirm"), false).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        unsafe {
            std::env::remove_var("RYOKAN_ANILIST_API_BASE");
            std::env::remove_var("JIKAN_API_BASE");
            std::env::remove_var("RYOKAN_KITSU_API_BASE");
        }
        anilist::reset_state_for_tests();
    }

    #[tokio::test]
    async fn confirm_refuses_when_nothing_would_be_written() {
        let tmp = tempfile::tempdir().unwrap();
        let media = tmp.path().join("media");
        std::fs::create_dir_all(&media).unwrap();
        let db = in_memory_pool().await;
        seed_media_root(&db, media.to_str().unwrap()).await;
        let state = build_test_app_state(db, None);
        let id = session_with_files(&state, &tmp.path().join("src"), false);
        let app = router(state.clone());

        let body = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("Nothing to import"), "{body}");
        let resp = post_empty(&app, &format!("/system/import/{id}/confirm"), true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("HX-Redirect").is_none());
        let body = body_text(resp).await;
        assert!(
            body.contains("Nothing to import: every file is excluded"),
            "{body}"
        );
        assert_eq!(
            session::get(&state.import_sessions, &id).unwrap().status,
            SessionStatus::Ready
        );
    }

    #[tokio::test]
    async fn confirm_refuses_without_media_root() {
        let tmp = tempfile::tempdir().unwrap();
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let id = session_with_files(&state, &tmp.path().join("src"), true);
        let app = router(state.clone());
        let resp = post_empty(&app, &format!("/system/import/{id}/confirm"), true).await;
        let body = body_text(resp).await;
        assert!(
            body.contains("Set a media root under Settings before importing"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn cancel_flags_only_a_running_import() {
        let tmp = tempfile::tempdir().unwrap();
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let id = session_with_files(&state, &tmp.path().join("src"), true);
        let app = router(state.clone());

        let resp = post_empty(&app, &format!("/system/import/{id}/cancel"), false).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(
            !session::get(&state.import_sessions, &id)
                .unwrap()
                .cancel
                .load(std::sync::atomic::Ordering::Relaxed),
            "Ready session: no-op"
        );

        session::update(&state.import_sessions, &id, |s| {
            s.status = SessionStatus::Importing
        });
        let body = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("import-importing"), "{body}");
        assert!(
            body.contains(&format!("data-import-progress=\"{id}-import\"")),
            "{body}"
        );
        let resp = post_empty(&app, &format!("/system/import/{id}/cancel"), true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            session::get(&state.import_sessions, &id)
                .unwrap()
                .cancel
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[tokio::test]
    async fn done_page_renders_the_report() {
        let tmp = tempfile::tempdir().unwrap();
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let id = session_with_files(&state, &tmp.path().join("src"), true);
        let report = ImportReport {
            series_created: 1,
            files_written: 1,
            files_failed: 1,
            bytes_written: 2048,
            groups: vec![manual_import::GroupReport {
                parsed_title: "Show".into(),
                series_title: "Show".into(),
                anilist_id: 100,
                series_id: Some(7),
                folder_name: "Show".into(),
                created: true,
                written: 1,
                errors: vec!["Show/Show - 02.mkv: permission denied".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        session::update(&state.import_sessions, &id, |s| {
            s.status = SessionStatus::Done(Box::new(report));
        });
        let app = router(state);
        let body = get_page(&app, &format!("/system/import?session={id}")).await;
        assert!(body.contains("Import finished with errors"), "{body}");
        assert!(body.contains("2.0 KiB"), "{body}");
        assert!(body.contains("/series/100"));
        assert!(body.contains("permission denied"));
        assert!(body.contains("Import another folder"));
    }
}
