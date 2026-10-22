use super::windows_browser_command;

const LOGIN_URLS: [&str; 2] = [
    "https://auth.x.ai/oauth2/authorize?response_type=code&client_id=fixture-client&redirect_uri=http%3A%2F%2F127.0.0.1%3A12345%2Fcallback&scope=openid+profile&code_challenge=ab^cd|ef(gh)<ij>&code_challenge_method=S256&state=a%26b",
    "https://cursor.com/loginDeepControl?challenge=ab^cd|ef(gh)<ij>&uuid=12345678-abcd-4321-abcd-123456789abc&mode=login&redirectTarget=cli%3A%2F%2Fcallback",
];

#[test]
fn oauth_browser_launch_keeps_urls_out_of_cmd() {
    for url in LOGIN_URLS {
        let (program, args) = windows_browser_command(url);
        assert_eq!(program, "rundll32.exe", "OAuth URLs must bypass cmd");
        assert_eq!(args, ["url.dll,FileProtocolHandler", url]);
    }
}

#[cfg(windows)]
#[test]
fn oauth_browser_launch_roundtrips_through_native_process() {
    // Substitute a native argv recorder for rundll32 so no browser is opened.
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("argv.rs");
    let recorder = temp.path().join("argv.exe");
    std::fs::write(
        &source,
        r#"fn main() {
            for arg in std::env::args().skip(1) {
                println!("{arg}");
            }
        }"#,
    )
    .unwrap();
    let compile = std::process::Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(&recorder)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "argv recorder did not compile: {}",
        String::from_utf8_lossy(&compile.stderr)
    );
    for url in LOGIN_URLS {
        let (program, args) = windows_browser_command(url);
        assert_eq!(program, "rundll32.exe", "OAuth URLs must bypass cmd");
        let output = std::process::Command::new(&recorder)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            stdout.lines().collect::<Vec<_>>(),
            ["url.dll,FileProtocolHandler", url]
        );
    }
}
