//! Entry points for fuzzing the code that handles untrusted input: the MyCode parser, form
//! decoding, theme import files and multipart uploads. Used by the `fuzz/` targets (cargo-fuzz,
//! nightly) and by randomized tests on stable. None of them may panic, hang or allocate without
//! bound, whatever the input.

use std::sync::LazyLock;

static PARSER_DATA: LazyLock<crate::parser::ParserData> = LazyLock::new(|| {
    crate::parser::ParserData::new(
        vec![crate::parser::Smilie {
            find: ":)".into(),
            image: "smile.png".into(),
            name: "Smile".into(),
        }],
        vec![("darn".into(), false, "****".into())],
        vec![(r"\[shout\](.*?)\[/shout\]".into(), "<b>$1</b>".into())],
    )
});

/// Parse a post with every feature on, plus the helpers that scan raw messages.
pub fn post(input: &str) {
    let opts = crate::parser::ParseOptions {
        me_username: Some("Tester".into()),
        ..Default::default()
    };
    let html = crate::parser::Parser::new(&PARSER_DATA, &opts).parse(input);
    // Output must never contain an active script tag.
    assert!(
        !html.to_ascii_lowercase().contains("<script"),
        "script tag in output"
    );
    let _ = crate::parser::to_plaintext(input);
    let _ = crate::parser::extract_mentions(input);
    let _ = crate::parser::extract_quoted_pids(input);
    let _ = crate::parser::count_images(input);
    let _ = crate::plugins::sanitize_html(&html);
}

/// Decode an urlencoded form body.
pub fn form(input: &[u8]) {
    let _ = crate::ctx::serde_html_form_parse::<std::collections::HashMap<String, serde_json::Value>>(
        input,
    );
}

/// Validate a theme import file.
pub fn theme_import(input: &str) {
    let _ =
        crate::domain::theme_export::parse(input, |n| n == "index.html" || n == "showthread.html");
}

/// Read a multipart body the way the upload routes do, with a small size limit per field.
pub fn multipart(input: &[u8]) {
    use axum::extract::FromRequest;
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let req = axum::http::Request::builder()
            .method("POST")
            .header(
                axum::http::header::CONTENT_TYPE,
                "multipart/form-data; boundary=fuzzboundary",
            )
            .body(axum::body::Body::from(input.to_vec()))
            .expect("request");
        let Ok(mut mp) = axum::extract::Multipart::from_request(req, &()).await else {
            return;
        };
        while let Ok(Some(mut field)) = mp.next_field().await {
            let _ = field.name();
            let _ = field.file_name();
            let mut total = 0usize;
            while let Ok(Some(chunk)) = field.chunk().await {
                total += chunk.len();
                if total > 64 * 1024 {
                    break;
                }
            }
        }
    });
}
