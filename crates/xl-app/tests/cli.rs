//! The `xl` binary's command-line modes print and exit without starting the
//! GUI (they run with no display at all here).

use std::process::Command;

fn xl() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_xl"));
    c.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY");
    c
}

#[test]
fn diff_and_textconv_run_headless() {
    let dir = std::env::temp_dir().join(format!("xl-bin-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a.csv"), dir.join("b.csv"));
    std::fs::write(&a, "Item,Qty\nCoffee,3\nMilk,6\n").unwrap();
    std::fs::write(&b, "Item,Qty\nCoffee,4\nTea,1\nMilk,6\n").unwrap();

    let out = xl().args(["diff", a.to_str().unwrap(), b.to_str().unwrap()]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let body: Vec<&str> = text.lines().skip(2).collect();
    assert_eq!(body, ["+ b row 3 inserted", "+   b!A3: \"Tea\"", "+   b!B3: 1", "b!B2: 3 → 4"], "{text}");

    let same = xl().args(["diff", a.to_str().unwrap(), a.to_str().unwrap()]).output().unwrap();
    assert_eq!((same.status.code(), same.stdout.len()), (Some(0), 0));

    let bad = xl().args(["diff", a.to_str().unwrap()]).output().unwrap();
    assert_eq!(bad.status.code(), Some(2));

    let conv = xl().args(["textconv", a.to_str().unwrap()]).output().unwrap();
    assert_eq!(conv.status.code(), Some(0));
    assert_eq!(String::from_utf8(conv.stdout).unwrap(), "# a\na!A1\tItem\na!B1\tQty\na!A2\tCoffee\na!B2\t3\na!A3\tMilk\na!B3\t6\n");
    std::fs::remove_dir_all(&dir).ok();
}
