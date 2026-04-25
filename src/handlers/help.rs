use askama::Template;
use axum::extract::State;
use axum::response::Html;

use crate::AppState;
use crate::models::config;

#[derive(Template)]
#[template(path = "help.html")]
struct HelpTemplate {
    page: String,
    title_language: String,
}

pub async fn help_page(State(state): State<AppState>) -> Html<String> {
    let template = HelpTemplate {
        page: "help".to_string(),
        title_language: config::get_title_language(&state.db).await,
    };
    Html(template.render().unwrap_or_default())
}
