//! End-to-end: run the binary against a checked-in fixture and check the
//! buckets, ordering and determinism of the output.

use std::process::{Command, Output};

const NOW: &str = "2026-10-09T08:00:00Z";

fn run(extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_anvil"))
        .args([
            "--config",
            "tests/fixtures/config.yaml",
            "--fixture",
            "tests/fixtures/search.json",
            "--now",
            NOW,
        ])
        .args(extra)
        .output()
        .expect("spawn anvil")
}

fn numbers(report: &serde_json::Value, section: &str) -> Vec<u64> {
    report[section]
        .as_array()
        .unwrap_or_else(|| panic!("section {section} missing"))
        .iter()
        .map(|e| e["pr"]["number"].as_u64().unwrap())
        .collect()
}

#[test]
fn fixture_lands_in_expected_buckets() {
    let out = run(&["--json", "--all"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    // 1: alice (team) in team repo, directly requested -> top.
    // 6: team-requested via query membership, tiny PR in another repo.
    // 5: carol pushed after my changes-requested review -> re-review, but
    //    420 lines, so the size factor pulls it under the tiny team request.
    // 10: bob's team PR, unreviewed, CI red -> listed but down-weighted.
    assert_eq!(numbers(&report, "review"), vec![1, 6, 5, 10]);

    // 2: my PR with failing CI; 8: approved & green -> merge.
    assert_eq!(numbers(&report, "action"), vec![2, 8]);

    // 3: my old stale spike; 7: dependabot untouched for weeks.
    assert_eq!(numbers(&report, "close"), vec![3, 7]);

    // 9: I already approved and nothing changed.
    assert_eq!(numbers(&report, "fyi"), vec![9]);

    // 4 is a draft and must not appear anywhere.
    for section in ["review", "action", "close", "fyi"] {
        assert!(
            !numbers(&report, section).contains(&4),
            "draft leaked into {section}"
        );
    }
}

#[test]
fn text_output_is_deterministic_and_readable() {
    let a = run(&["--explain"]);
    let b = run(&["--explain"]);
    assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
    assert_eq!(a.stdout, b.stdout);

    let text = String::from_utf8(a.stdout).unwrap();
    assert!(text.contains("NEEDS MY REVIEW"));
    assert!(text.contains("NEEDS MY ACTION"));
    assert!(text.contains("COULD BE CLOSED"));
    assert!(!text.contains("FYI"), "FYI only shows with --all");
    assert!(text.contains("acme/widgets#1"));
    assert!(text.contains("approved & green: merge"));
    assert!(
        text.contains("requested_me"),
        "--explain should list bonuses"
    );
    assert!(!text.contains("rewrite everything"), "draft must be hidden");
}

#[test]
fn missing_config_explains_itself() {
    let out = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .args(["--config", "tests/fixtures/does-not-exist.yaml"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no config file"), "{err}");
    assert!(err.contains("team:"), "should print an example: {err}");
}
