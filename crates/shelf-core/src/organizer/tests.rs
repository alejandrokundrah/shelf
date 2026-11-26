use super::*;
use std::fs;
use std::time::Instant;
use super::watcher::is_ignored;

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn png(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([(x * 7 % 256) as u8, (y * 13 % 256) as u8, ((x ^ y) % 256) as u8, 255])
    });
    let p = dir.join(name);
    img.save(&p).unwrap();
    p
}

fn organizer(dir: &Path) -> Organizer {
    Organizer::open_with(dir.join("data"), || {}, Duration::from_millis(40), Duration::from_millis(120))
}

// --- rules / sorting ---------------------------------------------------

#[test]
fn categories_by_extension() {
    assert_eq!(Category::of_ext("PDF"), Category::Pdfs);
    assert_eq!(Category::of_ext("exe"), Category::Installers);
    assert_eq!(Category::of_ext("zip"), Category::Archives);
    assert_eq!(Category::of_ext("JPEG"), Category::Images);
    assert_eq!(Category::of_ext("xyz"), Category::Other);
}

#[test]
fn default_rules_sort_by_type_and_apply() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().join("dl");
    fs::create_dir(&dl).unwrap();
    let a = write(&dl, "invoice.pdf", b"%PDF");
    let b = write(&dl, "setup.exe", b"MZ");
    let c = write(&dl, "mystery.xyz", b"?");
    let org = organizer(tmp.path());
    let (done, failed) = org.apply_all(&org.plan_paths(&[a, b, c.clone()], None, &dl));
    assert_eq!(done.len(), 2);
    assert!(failed.is_empty());
    assert!(dl.join("PDFs/invoice.pdf").exists());
    assert!(dl.join("Installers/setup.exe").exists());
    assert!(c.exists(), "unmatched files stay put");
}

#[test]
fn rule_without_conditions_matches_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let f = FileInfo::from_path(&write(tmp.path(), "a.pdf", b"x")).unwrap();
    let rule = Rule { id: 1, name: "x".into(), enabled: true, conditions: vec![], actions: vec![Action::MoveTo("X".into())] };
    assert!(!rule.matches(&f));
}

#[test]
fn size_and_name_conditions() {
    let tmp = tempfile::tempdir().unwrap();
    let f = FileInfo::from_path(&write(tmp.path(), "School-Notes.pdf", &vec![0u8; 3000])).unwrap();
    assert!(Condition::SizeOver(2000).matches(&f));
    assert!(!Condition::SizeUnder(2000).matches(&f));
    assert!(Condition::NameContains("school".into()).matches(&f));
    assert!(Condition::Extension("PDF".into()).matches(&f));
}

#[test]
fn jpeg_and_jpg_are_the_same_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let f = FileInfo::from_path(&write(tmp.path(), "a.jpeg", b"x")).unwrap();
    assert!(Condition::Extension("jpg".into()).matches(&f));
}

#[test]
fn collisions_get_numbered_names_not_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().to_path_buf();
    fs::create_dir(dl.join("PDFs")).unwrap();
    write(&dl.join("PDFs"), "a.pdf", b"old");
    let new = write(&dl, "a.pdf", b"new");
    let org = organizer(tmp.path());
    org.apply_all(&org.plan_paths(&[new], None, &dl));
    assert_eq!(fs::read(dl.join("PDFs/a.pdf")).unwrap(), b"old");
    assert_eq!(fs::read(dl.join("PDFs/a (1).pdf")).unwrap(), b"new");
}

#[test]
fn rename_template_and_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let info = FileInfo::from_path(&write(tmp.path(), "logo.png", b"x")).unwrap();
    let rule = Rule {
        id: 1,
        name: "r".into(),
        enabled: true,
        conditions: vec![Condition::Extension("png".into())],
        actions: vec![Action::Rename("{date}_{name}".into())],
    };
    let plan = build_plan(&info, tmp.path(), &rule, 0).unwrap();
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    assert_eq!(plan.dest.file_name().unwrap().to_string_lossy(), format!("{date}_logo.png"));
}

#[test]
fn rename_sanitizes_illegal_characters() {
    assert_eq!(sanitize_file_name("a:b*c?.png"), "a_b_c_.png");
}

#[test]
fn well_known_folders_resolve() {
    let base = Path::new("/base");
    assert_eq!(resolve_dest(base, Path::new("Study")), base.join("Study"));
    if let Some(pics) = dirs::picture_dir() {
        assert_eq!(resolve_dest(base, Path::new("Pictures/Designs")), pics.join("Designs"));
    }
}

// --- images ------------------------------------------------------------

#[test]
fn convert_png_to_webp_and_original_is_kept_for_undo() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().to_path_buf();
    let src = png(&dl, "logo.png", 64, 64);
    let original = fs::read(&src).unwrap();
    let rule = Rule {
        id: 1, name: "r".into(), enabled: true,
        conditions: vec![Condition::Category(Category::Images)],
        actions: vec![Action::Convert(ImageFormat::Webp), Action::MoveTo("Out".into())],
    };
    let org = organizer(tmp.path());
    let plans = org.plan_paths(&[src.clone()], Some(&[rule]), &dl);
    assert_eq!(plans.len(), 1);
    let (done, failed) = org.apply_all(&plans);
    assert!(failed.is_empty(), "{failed:?}");
    let out = dl.join("Out/logo.webp");
    let bytes = fs::read(&out).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
    assert!(!src.exists());

    org.undo(done[0].id).unwrap();
    assert_eq!(fs::read(&src).unwrap(), original, "original restored byte for byte");
    assert!(!out.exists());
    assert!(!dl.join("Out").exists(), "folder created by the action is removed");
}

#[test]
fn resize_keeps_aspect_and_never_enlarges() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().to_path_buf();
    let big = png(&dl, "big.png", 400, 200);
    let small = png(&dl, "small.png", 50, 50);
    let rule = Rule {
        id: 1, name: "r".into(), enabled: true,
        conditions: vec![Condition::Category(Category::Images)],
        actions: vec![Action::Resize { max_width: 100, max_height: 0 }, Action::MoveTo("R".into())],
    };
    let org = organizer(tmp.path());
    org.apply_all(&org.plan_paths(&[big, small], Some(&[rule]), &dl));
    let b = image::open(dl.join("R/big.png")).unwrap();
    assert_eq!((b.width(), b.height()), (100, 50));
    let s = image::open(dl.join("R/small.png")).unwrap();
    assert_eq!((s.width(), s.height()), (50, 50));
}

#[test]
fn jpeg_compress_shrinks_and_flattens_transparency() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().to_path_buf();
    let src = png(&dl, "photo.png", 200, 200);
    let rule = |q| Rule {
        id: 1, name: "r".into(), enabled: true,
        conditions: vec![Condition::Category(Category::Images)],
        actions: vec![Action::Convert(ImageFormat::Jpeg), Action::Compress { quality: q }, Action::MoveTo(format!("q{q}").into())],
    };
    let org = organizer(tmp.path());
    org.apply_all(&org.plan_paths(&[src.clone()], Some(&[rule(95)]), &dl));
    // second run needs the source again (first run moved it)
    let src2 = png(&dl, "photo.png", 200, 200);
    org.apply_all(&org.plan_paths(&[src2], Some(&[rule(20)]), &dl));
    let hi = fs::metadata(dl.join("q95/photo.jpg")).unwrap().len();
    let lo = fs::metadata(dl.join("q20/photo.jpg")).unwrap().len();
    assert!(lo < hi, "quality 20 ({lo}) should be smaller than 95 ({hi})");
}

#[test]
fn undecodable_image_fails_cleanly_and_leaves_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().to_path_buf();
    let bad = write(&dl, "broken.png", b"not a png");
    let rule = Rule {
        id: 1, name: "r".into(), enabled: true,
        conditions: vec![Condition::Category(Category::Images)],
        actions: vec![Action::Convert(ImageFormat::Webp), Action::MoveTo("Out".into())],
    };
    let org = organizer(tmp.path());
    let (done, failed) = org.apply_all(&org.plan_paths(&[bad.clone()], Some(&[rule]), &dl));
    assert!(done.is_empty());
    assert_eq!(failed.len(), 1);
    assert!(bad.exists());
    assert!(!dl.join("Out").exists(), "no folder is created for a failed file");
}

#[test]
fn image_steps_are_skipped_for_non_images() {
    let tmp = tempfile::tempdir().unwrap();
    let info = FileInfo::from_path(&write(tmp.path(), "a.pdf", b"x")).unwrap();
    let rule = Rule {
        id: 1, name: "r".into(), enabled: true,
        conditions: vec![Condition::Extension("pdf".into())],
        actions: vec![Action::Convert(ImageFormat::Webp), Action::MoveTo("X".into())],
    };
    let plan = build_plan(&info, tmp.path(), &rule, 0).unwrap();
    assert!(!plan.ops.has_work());
    assert_eq!(plan.dest.extension().unwrap(), "pdf");
}

#[test]
fn batch_rename_numbers_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().to_path_buf();
    let files: Vec<PathBuf> = (0..3).map(|i| write(&dl, &format!("img{i}.txt"), b"x")).collect();
    let rule = Rule {
        id: 1, name: "r".into(), enabled: true,
        conditions: vec![Condition::Extension("txt".into())],
        actions: vec![Action::Rename("shot_{n}".into()), Action::MoveTo("B".into())],
    };
    let org = organizer(tmp.path());
    org.apply_all(&org.plan_paths(&files, Some(&[rule]), &dl));
    for n in ["shot_001.txt", "shot_002.txt", "shot_003.txt"] {
        assert!(dl.join("B").join(n).exists(), "{n}");
    }
}

// --- undo --------------------------------------------------------------

#[test]
fn undo_last_walks_back_and_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().to_path_buf();
    let a = write(&dl, "a.pdf", b"a");
    let b = write(&dl, "b.pdf", b"b");
    {
        let org = organizer(tmp.path());
        org.apply_all(&org.plan_paths(&[a.clone(), b.clone()], None, &dl));
    }
    let org = organizer(tmp.path()); // reopened: journal is on disk
    assert_eq!(org.history().len(), 2);
    org.undo_last().unwrap();
    assert!(b.exists() && !a.exists());
    org.undo_last().unwrap();
    assert!(a.exists() && b.exists());
    assert!(!dl.join("PDFs").exists());
    assert!(org.undo_last().is_err(), "nothing left to undo");
    assert!(org.history().iter().all(|e| e.undone));
}

#[test]
fn undo_all_puts_everything_back() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().to_path_buf();
    let originals: Vec<(PathBuf, Vec<u8>)> = (0..5)
        .map(|i| {
            let bytes = format!("file {i}").into_bytes();
            (write(&dl, &format!("f{i}.pdf"), &bytes), bytes)
        })
        .collect();
    let org = organizer(tmp.path());
    let paths: Vec<PathBuf> = originals.iter().map(|(p, _)| p.clone()).collect();
    org.apply_all(&org.plan_paths(&paths, None, &dl));
    assert!(!originals[0].0.exists());

    let (undone, failed) = org.undo_all();
    assert_eq!((undone, failed.len()), (5, 0));
    for (path, bytes) in &originals {
        assert_eq!(&fs::read(path).unwrap(), bytes);
    }
    assert!(!dl.join("PDFs").exists(), "emptied folder is removed");
    assert_eq!(org.undo_all(), (0, vec![]), "nothing left to undo");
}

#[test]
fn undo_all_skips_what_cannot_be_undone_and_reports_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().to_path_buf();
    let a = write(&dl, "a.pdf", b"a");
    let b = write(&dl, "b.pdf", b"b");
    let org = organizer(tmp.path());
    let (done, _) = org.apply_all(&org.plan_paths(&[a.clone(), b.clone()], None, &dl));
    fs::remove_file(&done[0].dest).unwrap(); // a.pdf is gone for good
    let (undone, failed) = org.undo_all();
    assert_eq!(undone, 1);
    assert_eq!(failed.len(), 1);
    assert!(failed[0].starts_with("a.pdf"), "{failed:?}");
    assert!(b.exists());
}

#[test]
fn undo_refuses_if_file_moved_again_and_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().to_path_buf();
    let a = write(&dl, "a.pdf", b"a");
    let org = organizer(tmp.path());
    let (done, _) = org.apply_all(&org.plan_paths(&[a], None, &dl));
    fs::remove_file(&done[0].dest).unwrap();
    assert!(org.undo(done[0].id).is_err());
    assert!(!org.history()[0].undone);
}

#[test]
fn undo_does_not_overwrite_a_new_file_at_the_old_spot() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().to_path_buf();
    let a = write(&dl, "a.pdf", b"first");
    let org = organizer(tmp.path());
    let (done, _) = org.apply_all(&org.plan_paths(&[a.clone()], None, &dl));
    write(&dl, "a.pdf", b"second"); // user downloads another a.pdf
    org.undo(done[0].id).unwrap();
    assert_eq!(fs::read(&a).unwrap(), b"second");
    assert_eq!(fs::read(dl.join("a (1).pdf")).unwrap(), b"first");
}

// --- watcher -----------------------------------------------------------

fn wait_for(org: &Organizer, mut ok: impl FnMut(&OrganizerEvent) -> bool) -> Option<OrganizerEvent> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        for ev in org.poll() {
            if ok(&ev) {
                return Some(ev);
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    None
}

#[test]
fn watcher_auto_applies_new_files_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().join("dl");
    fs::create_dir(&dl).unwrap();
    let old = write(&dl, "old.pdf", b"old");
    let mut org = organizer(tmp.path());
    org.update_config(|c| {
        c.watch_folder = Some(dl.clone());
        c.watching = true;
        c.auto_apply = true;
    });
    std::thread::sleep(Duration::from_millis(200));
    write(&dl, "new.pdf", b"new");
    let ev = wait_for(&org, |e| matches!(e, OrganizerEvent::Applied(_))).expect("applied");
    if let OrganizerEvent::Applied(entry) = ev {
        assert_eq!(entry.dest, dl.join("PDFs/new.pdf"));
    }
    assert!(old.exists(), "files present before watching are left alone");
}

#[test]
fn watcher_preview_mode_only_proposes() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().join("dl");
    fs::create_dir(&dl).unwrap();
    let mut org = organizer(tmp.path());
    org.update_config(|c| {
        c.watch_folder = Some(dl.clone());
        c.watching = true;
        c.auto_apply = false;
    });
    std::thread::sleep(Duration::from_millis(200));
    let f = write(&dl, "x.zip", b"zip");
    let ev = wait_for(&org, |e| matches!(e, OrganizerEvent::Proposed(_))).expect("proposed");
    if let OrganizerEvent::Proposed(plan) = ev {
        assert_eq!(plan.dest, dl.join("Archives/x.zip"));
    }
    assert!(f.exists(), "preview must not touch the file");
    assert!(org.history().is_empty());
}

#[test]
fn watcher_ignores_partial_downloads() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().join("dl");
    fs::create_dir(&dl).unwrap();
    let mut org = organizer(tmp.path());
    org.update_config(|c| {
        c.watch_folder = Some(dl.clone());
        c.watching = true;
        c.auto_apply = true;
    });
    std::thread::sleep(Duration::from_millis(200));
    write(&dl, "big.zip.crdownload", b"partial");
    assert!(wait_for(&org, |_| true).is_none());
    assert!(is_ignored(Path::new("a/.hidden")));
    assert!(is_ignored(Path::new("a/~$doc.docx")));
}

#[test]
fn plan_existing_tidies_what_is_already_there() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().join("dl");
    fs::create_dir(&dl).unwrap();
    write(&dl, "a.pdf", b"x");
    write(&dl, "b.exe", b"x");
    write(&dl, "c.unknown", b"x");
    let mut org = organizer(tmp.path());
    org.update_config(|c| c.watch_folder = Some(dl.clone()));
    assert_eq!(org.plan_existing().len(), 2);
}

#[test]
fn config_persists() {
    let tmp = tempfile::tempdir().unwrap();
    {
        let mut org = organizer(tmp.path());
        org.update_config(|c| c.auto_apply = true);
    }
    assert!(organizer(tmp.path()).config().auto_apply);
}

// --- plain-English rules -----------------------------------------------

fn parse(s: &str) -> Rule {
    parse_rule(s).unwrap_or_else(|e| panic!("{s:?}: {e}"))
}

#[test]
fn nl_school_pdfs_in_study() {
    let r = parse("put school PDFs in Study");
    assert!(r.conditions.contains(&Condition::Category(Category::Pdfs)));
    assert!(r.conditions.contains(&Condition::NameContains("school".into())));
    assert_eq!(r.actions, vec![Action::MoveTo("Study".into())]);
}

#[test]
fn nl_the_spec_example() {
    let r = parse("If it's an image over 2 MB → convert to WebP, compress, then move to Pictures/Designs.");
    assert!(r.conditions.contains(&Condition::Category(Category::Images)));
    assert!(r.conditions.contains(&Condition::SizeOver(2 * 1024 * 1024)));
    assert!(r.actions.contains(&Action::Convert(ImageFormat::Webp)));
    assert!(r.actions.contains(&Action::Compress { quality: 80 }));
    assert!(r.actions.contains(&Action::MoveTo(PathBuf::from("Pictures").join("Designs"))));
}

#[test]
fn nl_png_to_jpg_keeps_source_extension() {
    let r = parse("convert png files to jpg");
    assert!(r.conditions.contains(&Condition::Extension("png".into())));
    assert!(r.actions.contains(&Action::Convert(ImageFormat::Jpeg)));
}

#[test]
fn nl_resize_and_rename_with_date() {
    let r = parse("resize screenshots to 1200px wide and rename with the date");
    assert!(r.conditions.contains(&Condition::Category(Category::Images)));
    assert!(r.conditions.contains(&Condition::NameContains("screenshot".into())));
    assert!(r.actions.contains(&Action::Resize { max_width: 1200, max_height: 0 }));
    assert!(r.actions.contains(&Action::Rename("{date}_{name}".into())));
}

#[test]
fn nl_dimensions_and_quality() {
    let r = parse("resize images to 800x600 and compress at quality 60");
    assert!(r.actions.contains(&Action::Resize { max_width: 800, max_height: 600 }));
    assert!(r.actions.contains(&Action::Compress { quality: 60 }));
}

#[test]
fn nl_word_only_rule() {
    let r = parse("put invoices in Finance");
    assert_eq!(r.conditions, vec![Condition::NameContains("invoice".into())]);
    assert_eq!(r.actions, vec![Action::MoveTo("Finance".into())]);
}

#[test]
fn nl_folder_wording_and_small_size() {
    let r = parse("move installers under 50 MB into the Software folder");
    assert!(r.conditions.contains(&Condition::Category(Category::Installers)));
    assert!(r.conditions.contains(&Condition::SizeUnder(50 * 1024 * 1024)));
    assert_eq!(r.actions, vec![Action::MoveTo("Software".into())]);
}

#[test]
fn nl_image_actions_imply_images() {
    let r = parse("compress anything over 5 MB");
    assert!(r.conditions.contains(&Condition::Category(Category::Images)));
}

#[test]
fn nl_errors_are_helpful() {
    assert!(parse_rule("").is_err());
    assert!(parse_rule("hello there").unwrap_err().contains("what to do"));
    assert!(parse_rule("move to Study").unwrap_err().contains("which files"));
}

#[test]
fn nl_never_escapes_the_base_folder() {
    let r = parse("move pdfs to ../../Windows/System32");
    let Some(Action::MoveTo(p)) = r.actions.first() else { panic!() };
    assert!(!p.components().any(|c| c.as_os_str() == ".."));
}

#[test]
fn describe_is_readable() {
    let r = parse("put school PDFs in Study");
    let d = r.describe();
    assert!(d.contains("PDFs") && d.contains("school") && d.contains("Study"), "{d}");
}

// --- more file types ---------------------------------------------------

#[test]
fn design_code_3d_and_font_files_are_categorised() {
    for e in ["psd", "AI", "afphoto", "afdesign", "af", "fig", "xd", "sketch"] {
        assert_eq!(Category::of_ext(e), Category::Design, "{e}");
    }
    for e in ["rs", "ts", "py", "js", "html", "json", "sh"] {
        assert_eq!(Category::of_ext(e), Category::Code, "{e}");
    }
    for e in ["glb", "gltf", "obj", "fbx", "stl", "blend"] {
        assert_eq!(Category::of_ext(e), Category::Models3d, "{e}");
    }
    for e in ["ttf", "otf", "woff", "woff2"] {
        assert_eq!(Category::of_ext(e), Category::Fonts, "{e}");
    }
}

#[test]
fn new_categories_get_sorted_by_default_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let dl = tmp.path().join("dl");
    fs::create_dir(&dl).unwrap();
    let files: Vec<PathBuf> = ["poster.psd", "logo.afdesign", "main.rs", "model.glb", "Inter.ttf"]
        .iter()
        .map(|n| write(&dl, n, b"x"))
        .collect();
    let org = organizer(tmp.path());
    let (done, failed) = org.apply_all(&org.plan_paths(&files, None, &dl));
    assert_eq!(done.len(), 5);
    assert!(failed.is_empty());
    for (folder, name) in [("Design", "poster.psd"), ("Design", "logo.afdesign"), ("Code", "main.rs"), ("3D", "model.glb"), ("Fonts", "Inter.ttf")] {
        assert!(dl.join(folder).join(name).exists(), "{folder}/{name}");
    }
}

#[test]
fn old_saved_config_gains_the_new_default_rules_without_losing_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    fs::create_dir(&data).unwrap();
    // A v1 config: only the original seven sort rules, one of them customised, plus a user rule.
    let mut old = default_rules().into_iter().take(7).collect::<Vec<_>>();
    old[1].actions = vec![Action::MoveTo("My Pictures".into())];
    old.insert(0, Rule {
        id: 99, name: "mine".into(), enabled: true,
        conditions: vec![Condition::NameContains("school".into())],
        actions: vec![Action::MoveTo("Study".into())],
    });
    let json = serde_json::json!({ "watch_folder": null, "watching": false, "auto_apply": false, "rules": old });
    fs::write(data.join("config.json"), serde_json::to_vec(&json).unwrap()).unwrap();

    let org = organizer(tmp.path());
    let cfg = org.config();
    let has = |c: Category| cfg.rules.iter().any(|r| r.conditions == vec![Condition::Category(c)]);
    assert!(has(Category::Design) && has(Category::Code) && has(Category::Models3d) && has(Category::Fonts));
    assert_eq!(cfg.rules[0].name, "mine", "user rule keeps its place");
    assert_eq!(cfg.rules[2].actions, vec![Action::MoveTo("My Pictures".into())], "edit kept");
    let ids: std::collections::HashSet<u64> = cfg.rules.iter().map(|r| r.id).collect();
    assert_eq!(ids.len(), cfg.rules.len(), "rule ids stay unique");
    // Reopening does not add them twice.
    drop(org);
    assert_eq!(organizer(tmp.path()).config().rules.len(), cfg.rules.len());
}

#[test]
fn nl_understands_the_new_types() {
    assert!(parse("put fonts in Fonts").conditions.contains(&Condition::Category(Category::Fonts)));
    assert!(parse("move 3d models to Assets").conditions.contains(&Condition::Category(Category::Models3d)));
    assert!(parse("sort code into Projects").conditions.contains(&Condition::Category(Category::Code)));
    assert!(parse("put psd files in Work").conditions.contains(&Condition::Category(Category::Design)));
}
