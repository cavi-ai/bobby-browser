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
    // A whole-page inspect selects the document root, whose text includes
    // <head>. Head chrome such as <title> must not make an otherwise empty
    // client-rendered shell look like a complete direct-HTTP response.
    let shell_probe = if command.selector.is_none() && command.target.is_none() {
        let body_selector = Selector::parse("body").expect("static selector");
        document
            .select(&body_selector)
            .next()
            .map(|body| {
                body.text()
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    } else {
        text.clone()
    };
    let meaningful = scripts.iter().fold(shell_probe, |remaining, script| {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_page_spa_shell_with_head_text_requires_javascript() {
        let html = r#"<!doctype html><html><head><title>Northstar Ops</title>
<meta name="description" content="workspace"></head>
<body><div id="app"></div><script type="module" src="/app.js"></script></body></html>"#;

        let result = inspect_document(
            "https://example.test/?run=secret",
            html,
            &InspectCommand::default(),
        );

        assert_eq!(result, Err(ExecutionReason::JavascriptRequired));
    }

    #[test]
    fn whole_page_static_body_text_remains_direct_http_eligible() {
        let html = r#"<!doctype html><html><head><title>Report</title></head>
<body><h1>Quarterly</h1><p>All green</p><script src="/enhance.js"></script></body></html>"#;

        let evidence = inspect_document("https://example.test/", html, &InspectCommand::default())
            .expect("body copy makes the direct response meaningful");

        match evidence {
            Evidence::Inspection { text, .. } => assert!(text.contains("All green")),
            other => panic!("unexpected evidence: {other:?}"),
        }
    }
}
