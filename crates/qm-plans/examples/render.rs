//! Legacy: `cargo run -p qm-plans --example render -- <folder> [final|approved|locked]`.
//! Authored HTML: add `--authored` after the folder. Reads `plan.html`, writes `review.html`
//! without modifying the authored source. With `--starter`, writes a fresh starter to stdout.

use qm_plans::{Assets, Meta, PageState, render};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(dir) = args.next().map(PathBuf::from) else {
        eprintln!(
            "usage: render <plan folder> [--authored] [final|approved|locked]; render --starter"
        );
        return ExitCode::FAILURE;
    };
    if dir.as_os_str() == "--starter" {
        print!("{}", qm_plans::authoring_template());
        return ExitCode::SUCCESS;
    }
    let mut option = args.next();
    let authored = option.as_deref() == Some("--authored");
    if authored {
        option = args.next();
    }
    let state = match option.as_deref() {
        Some("final") => PageState::Final,
        Some("approved") => PageState::Approved { at: "14:02".into() },
        Some("locked") => PageState::Locked {
            reason: "Autonomy changed to auto.".into(),
        },
        _ => PageState::Open,
    };
    let source = if authored {
        qm_plans::PAGE_FILE
    } else {
        qm_plans::PLAN_FILE
    };
    let md = match std::fs::read_to_string(dir.join(source)) {
        Ok(md) => md,
        Err(e) => {
            eprintln!("couldn't read {source}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let assets = match Assets::load(&dir) {
        Ok(assets) => assets,
        Err(problems) => {
            eprintln!("{problems}");
            return ExitCode::FAILURE;
        }
    };
    let meta = Meta {
        task: "T-0001".into(),
        task_label: None,
        plan_uid: "0123456789abcdef01234567".into(),
        round: 1,
        project: "example".into(),
        state,
        previous: BTreeMap::new(),
        submit: None,
    };
    let result = if authored {
        qm_plans::compose_html(&md, &assets, &meta)
    } else {
        render(&md, &assets, &meta)
    };
    match result {
        Ok(page) => {
            let out = dir.join(if authored {
                "review.html"
            } else {
                qm_plans::PAGE_FILE
            });
            if let Err(e) = std::fs::write(&out, page.html) {
                eprintln!("couldn't write {}: {e}", out.display());
                return ExitCode::FAILURE;
            }
            println!("{}\n{}", out.display(), page.checked.summary());
            ExitCode::SUCCESS
        }
        Err(problems) => {
            eprintln!("{problems}");
            ExitCode::FAILURE
        }
    }
}
