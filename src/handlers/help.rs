use askama::Template;
use axum::response::Html;

#[derive(Template)]
#[template(path = "help.html")]
struct HelpTemplate {
    page: String,
}

pub async fn help_page() -> Html<String> {
    let template = HelpTemplate {
        page: "help".to_string(),
    };
    Html(template.render().unwrap_or_default())
}
