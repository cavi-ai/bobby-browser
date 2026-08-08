use scraper::{Html, Selector};
use types::{Evidence, ExecutionReason, InspectCommand};

pub(crate) fn inspect_document(
    url: &str,
    body: &str,
    command: &InspectCommand,
) -> Result<Evidence, ExecutionReason> {
    let document = Html::parse_document(body);
    let title_selector = Selector::parse("title").expect("static selector");
    let title = document
        .select(&title_selector)
        .next()
        .map(|node| node.text().collect::<String>().trim().to_owned())
        .unwrap_or_default();

    let selected = if let Some(selector) = command.selector.as_deref() {
        let selector =
            Selector::parse(selector).map_err(|_| ExecutionReason::JavascriptRequired)?;
        document
            .select(&selector)
            .next()
            .ok_or(ExecutionReason::JavascriptRequired)?
    } else {
        document.root_element()
    };
    let text = selected
        .text()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let script_selector = Selector::parse("script").expect("static selector");
    let scripts = document.select(&script_selector).collect::<Vec<_>>();
    let meaningful = scripts.iter().fold(text.clone(), |remaining, script| {
        remaining.replace(&script.text().collect::<String>(), "")
    });
    if meaningful.trim().is_empty() && !scripts.is_empty() {
        return Err(ExecutionReason::JavascriptRequired);
    }

    Ok(Evidence::Inspection {
        selector: command.selector.clone(),
        url: url.to_owned(),
        title,
        text,
        html: command.include_html.then(|| selected.html()),
    })
}
