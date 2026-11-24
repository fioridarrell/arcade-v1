//! The security warning from docs/proposals/SECURITY-HARDENING-345.md section 5:
//! the README (the install page in this repo) leads with it, SECURITY.md says what the
//! service can reach, and the Windows installer will not continue until that screen is acknowledged.

use std::path::Path;

const NOTICE: [&str; 4] = [
    "Arcade V1 is beta software with no independent security audit yet.",
    "Do not use funds you can't afford to lose.",
    "Do not restore a seed phrase that holds real savings.",
    "It installs a background service that starts at login and connects to other arcades.",
];

fn repo(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn has_notice(text: &str) -> bool {
    NOTICE.iter().all(|sentence| text.contains(sentence))
}

/// Whether `script` installs only after the warning screen is acknowledged.
///
/// The screen is the license page, and only while `MUI_LICENSEPAGE_CHECKBOX` is set before it:
/// Next stays disabled until the box is checked, and that page is inserted before the files are
/// unpacked. Silent install (`/S`) never shows the screen, and `.onInit` aborts that path.
/// A script that drops any of those does not count as acknowledged, so this returns false.
fn installer_may_proceed(script: &str, warning: &str, acknowledged: bool, silent: bool) -> bool {
    if !has_notice(warning) {
        return false;
    }
    let Some(license) = script.find("!insertmacro MUI_PAGE_LICENSE") else {
        return false;
    };
    let Some(files) = script.find("!insertmacro MUI_PAGE_INSTFILES") else {
        return false;
    };
    if license > files {
        return false;
    }
    let before_screen = &script[..license];
    let license_line_end = script[license..]
        .find('\n')
        .map(|n| license + n)
        .unwrap_or(script.len());
    // The checkbox define, not the CHECKBOX_TEXT line that shares its prefix.
    let checkbox = before_screen
        .lines()
        .any(|line| line.trim() == "!define MUI_LICENSEPAGE_CHECKBOX");
    let undefined = before_screen
        .lines()
        .any(|line| line.trim().starts_with("!undef MUI_LICENSEPAGE_CHECKBOX"));
    if !checkbox || undefined {
        return false;
    }
    // The warning file is named on the license-page line itself.
    if !script[license..license_line_end].contains("SECURITY-WARNING.txt") {
        return false;
    }
    let Some(refused) = script.find("warning_silent_refused:") else {
        return false;
    };
    let Some(shown) = script.find("warning_screen_shown:") else {
        return false;
    };
    let silent_branch = &script[refused..shown];
    if !script.contains("IfSilent warning_silent_refused warning_screen_shown")
        || !silent_branch.contains("Abort ")
    {
        return false;
    }
    !silent && acknowledged
}

#[test]
fn the_readme_opens_with_the_security_warning() {
    let readme = repo("README.md");
    let top = readme
        .split("Try it before you install anything")
        .next()
        .expect("try-it note");
    assert!(
        has_notice(top),
        "the warning belongs at the top of the README, before the try-it note"
    );
    assert!(
        top.contains("](SECURITY.md)"),
        "the top of the README links to SECURITY.md"
    );
    assert!(top.contains("team@arcade-nodes.com"));
}

#[test]
fn the_install_section_opens_with_the_security_warning() {
    let readme = repo("README.md");
    let install = readme
        .split("## Install on Linux")
        .nth(1)
        .expect("install section");
    let before_command = install.split("curl -fsSL").next().expect("install command");
    assert!(
        has_notice(before_command),
        "the install section leads with the warning, before the install command"
    );
    assert!(before_command.contains("SECURITY.md"));
}

#[test]
fn the_windows_install_section_opens_with_the_security_warning() {
    let readme = repo("README.md");
    let install = readme
        .split("## Install on Windows")
        .nth(1)
        .expect("windows install section");
    let before_command = install
        .split("Paste this into PowerShell")
        .next()
        .expect("windows command");
    assert!(
        has_notice(before_command),
        "the Windows install section leads with the warning, before the install command"
    );
    assert!(before_command.contains("SECURITY.md"));
}

#[test]
fn the_release_notes_lead_with_the_warning() {
    let notes = repo(".github/workflows/release.yml");
    let lead = notes
        .split("--notes ")
        .nth(1)
        .expect("release notes")
        .split("Every download here")
        .next()
        .unwrap();
    assert!(
        has_notice(lead),
        "the release page (where Setup.exe is downloaded) leads with the warning"
    );
    assert!(lead.contains("SECURITY.md") && lead.contains("team@arcade-nodes.com"));
}

#[test]
fn security_md_covers_access_ports_updates_and_how_to_report() {
    let doc = repo("SECURITY.md");
    assert!(has_notice(&doc), "SECURITY.md opens with the same warning");
    for required in [
        "team@arcade-nodes.com",
        "8420",
        "8421",
        "3478",
        "127.0.0.1",
        "secret.key",
        "12-word",
        "Strawberry",
        "SHA-256",
        "off",
        ".prev",
    ] {
        assert!(
            doc.contains(required),
            "SECURITY.md should mention {required}"
        );
    }
    assert!(
        doc.contains("0.0.0.0"),
        "mesh and STUN listen beyond loopback; the doc should say so"
    );
    assert!(
        doc.contains("does not run as root")
            || doc.contains("not as root")
            || doc.contains("not run as root")
    );
}

#[test]
fn the_windows_installer_will_not_proceed_without_the_warning() {
    let script = repo("scripts/windows/setup.nsi");
    let warning = repo("scripts/windows/SECURITY-WARNING.txt");
    assert!(
        installer_may_proceed(&script, &warning, true, false),
        "checking the box on the warning screen is what lets the install continue"
    );
    assert!(
        !installer_may_proceed(&script, &warning, false, false),
        "the installer must not proceed until the warning screen is acknowledged"
    );
    assert!(
        !installer_may_proceed(&script, &warning, true, true),
        "silent install skips the screen and must be refused"
    );
    assert!(!installer_may_proceed(&script, &warning, false, true));

    let no_box = script.replacen("!define MUI_LICENSEPAGE_CHECKBOX\n", "", 1);
    assert!(
        !installer_may_proceed(&no_box, &warning, true, false),
        "without the checkbox the screen does not count as acknowledged"
    );
    let no_screen = script.replacen(
        "!insertmacro MUI_PAGE_LICENSE \"${__FILEDIR__}/SECURITY-WARNING.txt\"\n",
        "",
        1,
    );
    assert!(
        !installer_may_proceed(&no_screen, &warning, true, false),
        "with no warning screen the install must not count as acknowledged"
    );
}
