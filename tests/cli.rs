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

#[test]
fn top_caps_each_section_and_reports_the_rest() {
    let out = run(&["--top", "1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("acme/widgets#1"),
        "best review PR still shown"
    );
    assert!(
        !text.contains("other/repo#6"),
        "second review PR cut by --top 1"
    );
    assert!(text.contains("… and 3 more"), "{text}");

    let json = run(&["--json", "--top", "1"]);
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(numbers(&report, "review"), vec![1]);
    assert_eq!(numbers(&report, "close"), vec![3]);

    let unlimited = run(&["--json", "--top", "0"]);
    let report: serde_json::Value = serde_json::from_slice(&unlimited.stdout).unwrap();
    assert_eq!(numbers(&report, "review").len(), 4);
}

#[test]
fn min_review_score_hides_weak_candidates() {
    let base = [
        "--config",
        "tests/fixtures/config-threshold.yaml",
        "--fixture",
        "tests/fixtures/search.json",
        "--now",
        NOW,
    ];
    let out = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .args(base)
        .args(["--json", "--all"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // 5 (~38) and 10 (~30) fall below 40; 1 and 6 stay. FYI 9 is also below.
    assert_eq!(numbers(&report, "review"), vec![1, 6]);
    assert_eq!(numbers(&report, "fyi"), Vec::<u64>::new());
    assert_eq!(report["hidden_below_threshold"], 3);
    // Action and close sections are unaffected by the review threshold.
    assert_eq!(numbers(&report, "action"), vec![2, 8]);
    assert_eq!(numbers(&report, "close"), vec![3, 7]);

    let text = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .args(base)
        .output()
        .unwrap();
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("3 hidden for scoring below 40"), "{text}");
}
