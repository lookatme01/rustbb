//! Randomized inputs through the fuzzing entry points on stable (the `fuzz/` targets run the
//! same functions under libFuzzer in CI). Any panic is a bug.

use proptest::prelude::*;

/// Strings biased towards markup the parser cares about.
fn markup() -> impl Strategy<Value = String> {
    let atoms = prop_oneof![
        Just("[b]".to_string()),
        Just("[/b]".to_string()),
        Just("[quote=".to_string()),
        Just("[/quote]".to_string()),
        Just("[url=".to_string()),
        Just("[/url]".to_string()),
        Just("[img]".to_string()),
        Just("[/img]".to_string()),
        Just("[code]".to_string()),
        Just("[/code]".to_string()),
        Just("[list]".to_string()),
        Just("[*]".to_string()),
        Just("[video=youtube]".to_string()),
        Just("[color=red]".to_string()),
        Just("]".to_string()),
        Just("<script>".to_string()),
        Just("javascript:".to_string()),
        Just("@\"".to_string()),
        Just(":)".to_string()),
        Just("&amp;".to_string()),
        Just("\n".to_string()),
        Just("\"".to_string()),
        Just("'".to_string()),
        ".{0,8}",
    ];
    prop::collection::vec(atoms, 0..40).prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, failure_persistence: None, .. ProptestConfig::default() })]

    #[test]
    fn parser_never_panics(s in markup()) {
        rbb::fuzzing::post(&s);
    }

    #[test]
    fn arbitrary_text_parses(s in ".{0,300}") {
        rbb::fuzzing::post(&s);
    }

    #[test]
    fn forms_never_panic(b in prop::collection::vec(any::<u8>(), 0..300)) {
        rbb::fuzzing::form(&b);
    }

    #[test]
    fn theme_files_never_panic(s in ".{0,300}") {
        rbb::fuzzing::theme_import(&s);
        rbb::fuzzing::theme_import(&format!("{{\"rbb_theme\":1,\"templates\":{{\"index.html\":{s:?}}}}}"));
    }

    #[test]
    fn multipart_never_panics(b in prop::collection::vec(any::<u8>(), 0..400)) {
        let mut body = b"--fuzzboundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a\"\r\n\r\n".to_vec();
        body.extend_from_slice(&b);
        rbb::fuzzing::multipart(&b);
        rbb::fuzzing::multipart(&body);
    }
}
