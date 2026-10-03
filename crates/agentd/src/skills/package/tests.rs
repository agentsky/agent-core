use std::io::Cursor;
use std::time::{Duration, Instant};

use testkit::TempDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::*;

const SKILL: &str = "---\nname: pdf-tools\ndescription: Read and fill PDF forms.\n---\n# PDF\n";

fn problem(result: Result<impl std::fmt::Debug, CheckError>) -> Problem {
    match result {
        Err(CheckError::Problem(problem)) => problem,
        other => panic!("expected a problem, got {other:?}"),
    }
}

/// A zip of `entries`: a name, its contents (`None` for a directory) and
/// its Unix mode.
fn zip_of(entries: &[(&str, Option<&[u8]>, u32)]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, contents, mode) in entries {
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(*mode);
        match contents {
            Some(bytes) => {
                zip.start_file(*name, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
            None => zip.add_directory(*name, options).unwrap(),
        }
    }
    zip.finish().unwrap().into_inner()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

#[test]
fn front_matter_gives_the_name_description_and_hosts() {
    let manifest = parse_skill_file(SKILL).unwrap();
    assert_eq!(manifest.name.as_str(), "pdf-tools");
    assert_eq!(manifest.description, "Read and fill PDF forms.");
    assert!(manifest.hosts.is_empty());

    let text = "\u{feff}---\r\nname: gh\r\ndescription: >\r\n  Work with\r\n  GitHub.\r\n\
                allowed-hosts:\r\n  - api.github.com\r\n  - API.GitHub.com.\r\n  - raw.githubusercontent.com\r\n  - git.example.org:8443\r\n\
                allowed-tools: Bash\r\n---\r\nbody\r\n";
    let manifest = parse_skill_file(text).unwrap();
    assert_eq!(manifest.name.as_str(), "gh");
    assert_eq!(manifest.description, "Work with GitHub.");
    let hosts: Vec<String> = manifest.hosts.iter().map(ToString::to_string).collect();
    assert_eq!(
        hosts,
        [
            "api.github.com",
            "raw.githubusercontent.com",
            "git.example.org:8443"
        ],
        "hosts are normalized and repeats dropped"
    );

    let line =
        "---\nname: x\ndescription: y\nallowed-hosts: pypi.org, files.pythonhosted.org\n---\n";
    let hosts: Vec<String> = parse_skill_file(line)
        .unwrap()
        .hosts
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(hosts, ["pypi.org", "files.pythonhosted.org"]);
}

#[test]
fn front_matter_is_required_and_bounded() {
    for text in [
        "",
        "# no front matter\n",
        "name: x\n---\n",
        "---\nname: x\ndescription: y\n",
        "--- \nname: x\n",
        "---\nname: x\ndescription: y\n...\n",
    ] {
        assert_eq!(
            parse_skill_file(text),
            Err(Problem::NoFrontMatter),
            "{text:?}"
        );
    }
    let long = format!(
        "---\nname: x\ndescription: y\nnotes: {}\n---\n",
        "z".repeat(MAX_FRONT_MATTER_BYTES)
    );
    assert_eq!(parse_skill_file(&long), Err(Problem::NoFrontMatter));
    assert_eq!(parse_skill_file("---\n---\n"), Err(Problem::Name));
    assert_eq!(
        parse_skill_file("---\nname: [x\n---\n"),
        Err(Problem::FrontMatter)
    );
    assert_eq!(
        parse_skill_file("---\nname:\n  nested: map\ndescription: y\n---\n"),
        Err(Problem::FrontMatter)
    );
    assert_eq!(
        parse_skill_file("---\nname: x\ndescription: y\nallowed-hosts: {a: b}\n---\n"),
        Err(Problem::FrontMatter)
    );
}

#[test]
fn names_and_descriptions_follow_claude_codes_rules() {
    let with = |name: &str, description: &str| {
        parse_skill_file(&format!(
            "---\nname: {name}\ndescription: {description}\n---\n"
        ))
    };
    for bad in ["PDF", "pdf_tools", "../x", "a/b", "''", &"x".repeat(65)] {
        assert_eq!(with(bad, "y"), Err(Problem::Name), "{bad}");
    }
    assert_eq!(
        parse_skill_file("---\ndescription: y\n---\n"),
        Err(Problem::Name)
    );
    assert_eq!(with("agentctl", "y"), Err(Problem::Reserved));
    assert_eq!(with("x", "''"), Err(Problem::Description));
    assert_eq!(
        parse_skill_file("---\nname: x\n---\n"),
        Err(Problem::Description)
    );
    assert!(with("x", &"d".repeat(MAX_DESCRIPTION_CHARS)).is_ok());
    assert_eq!(
        with("x", &"d".repeat(MAX_DESCRIPTION_CHARS + 1)),
        Err(Problem::Description)
    );
}

#[test]
fn hosts_follow_the_egress_rules() {
    let with = |hosts: &str| {
        parse_skill_file(&format!(
            "---\nname: x\ndescription: y\nallowed-hosts: [{hosts}]\n---\n"
        ))
    };
    for (bad, index) in [
        ("api.anthropic.com", 1),
        ("github.com, 169.254.169.254", 2),
        ("localhost", 1),
        ("'*.com'", 1),
        ("github.com, '*.github.io'", 2),
        ("'*.githubusercontent.com'", 1),
        ("'10.0.0.1'", 1),
        ("'[::1]'", 1),
        ("'github.com:0'", 1),
        ("'github.com/path'", 1),
    ] {
        let err = with(bad).unwrap_err();
        let Problem::Host { index: at, reason } = &err else {
            panic!("{bad}: {err:?}");
        };
        assert_eq!(*at, index, "{bad}");
        assert!(!reason.contains("169.254"), "the reason repeats no input");
        assert!(err.to_string().starts_with(&format!("Entry {index} of")));
    }
    let Err(Problem::Host { reason, .. }) = with("'*.example.org:8443'") else {
        panic!("a wildcard");
    };
    assert!(reason.contains("wildcards aren't allowed"), "{reason}");
    let many: Vec<String> = (0..=MAX_HOSTS)
        .map(|i| format!("h{i}.example.org"))
        .collect();
    assert_eq!(with(&many.join(", ")), Err(Problem::TooManyHosts));
    assert_eq!(
        with(&many[..MAX_HOSTS].join(", ")).unwrap().hosts.len(),
        MAX_HOSTS
    );
}

#[test]
fn alias_expansion_is_cheap_or_refused() {
    let mut bomb = String::from("a0: &a0 [lol, lol, lol, lol, lol, lol, lol, lol, lol]\n");
    for i in 1..10 {
        let prev = i - 1;
        let refs = vec![format!("*a{prev}"); 9].join(", ");
        bomb.push_str(&format!("a{i}: &a{i} [{refs}]\n"));
    }
    let started = Instant::now();
    let ignored = format!("---\nname: x\ndescription: y\n{bomb}---\n");
    assert!(
        parse_skill_file(&ignored).is_ok(),
        "keys it doesn't read are skipped"
    );
    let hosts = format!("---\nname: x\ndescription: y\n{bomb}allowed-hosts: *a9\n---\n");
    assert_eq!(parse_skill_file(&hosts), Err(Problem::FrontMatter));
    let description = format!("---\nname: x\n{bomb}description: *a9\n---\n");
    assert_eq!(parse_skill_file(&description), Err(Problem::FrontMatter));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_zip_unpacks_with_plain_modes_and_skips_macos_metadata() {
    let dir = TempDir::new("agentd-skill");
    let bytes = zip_of(&[
        ("pdf-tools/", None, 0o40755),
        ("pdf-tools/SKILL.md", Some(SKILL.as_bytes()), 0o100666),
        ("pdf-tools/scripts/", None, 0o40777),
        ("pdf-tools/scripts/fill.sh", Some(b"#!/bin/sh\n"), 0o106777),
        ("__MACOSX/pdf-tools/._SKILL.md", Some(b"junk"), 0o100644),
    ]);
    let out = dir.join("out");
    unpack_zip(&bytes, &out).unwrap();
    check_tree(&out).unwrap();
    let (root, manifest) = find_skill(&out).unwrap();
    assert_eq!(root, out.join("pdf-tools"));
    assert_eq!(manifest.name.as_str(), "pdf-tools");
    assert!(!out.join("__MACOSX").exists());
    assert_eq!(mode(&root.join("SKILL.md")), 0o644);
    assert_eq!(mode(&root.join("scripts")), 0o755);
    assert_eq!(
        mode(&root.join("scripts/fill.sh")),
        0o755,
        "the execute bit stays, set-id and write bits go"
    );
}

#[test]
fn zip_entries_that_leave_the_directory_or_hide_are_refused() {
    for name in [
        "../escape",
        "a/../../escape",
        "/etc/passwd",
        "a//b",
        "./a",
        "a\\..\\b",
        "a\u{202e}txt.exe",
        "a\u{fe0f}b",
        "a\u{3164}b",
        "a\nb",
        " ",
        "a/ /b",
        "\u{3000}/b",
    ] {
        let dir = TempDir::new("agentd-skill");
        let bytes = zip_of(&[(name, Some(b"x"), 0o100644)]);
        assert_eq!(
            problem(unpack_zip(&bytes, &dir.join("out"))),
            Problem::BadName,
            "{name:?}"
        );
        assert!(!dir.path().parent().unwrap().join("escape").exists());
    }
}

#[test]
fn zip_symlinks_duplicates_and_special_files_are_refused() {
    let dir = TempDir::new("agentd-skill");
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.add_symlink("link", "/etc/passwd", SimpleFileOptions::default())
        .unwrap();
    let bytes = zip.finish().unwrap().into_inner();
    assert_eq!(
        problem(unpack_zip(&bytes, &dir.join("a"))),
        Problem::Symlink
    );

    let bytes = zip_of(&[("x", Some(b"1"), 0o100644), ("x/y", Some(b"2"), 0o100644)]);
    assert_eq!(
        problem(unpack_zip(&bytes, &dir.join("b"))),
        Problem::Duplicate
    );
    let bytes = zip_of(&[("x/", None, 0o40755), ("x", Some(b"2"), 0o100644)]);
    assert_eq!(
        problem(unpack_zip(&bytes, &dir.join("c"))),
        Problem::Duplicate
    );
    let mut bytes = zip_of(&[("fifo", Some(b""), 0o100644)]);
    let central = bytes
        .windows(4)
        .rposition(|w| w == [0x50, 0x4b, 0x01, 0x02])
        .unwrap();
    bytes[central + 5] = 3;
    bytes[central + 38..central + 42].copy_from_slice(&(0o010_644u32 << 16).to_le_bytes());
    assert_eq!(
        problem(unpack_zip(&bytes, &dir.join("d"))),
        Problem::SpecialFile
    );
    assert_eq!(
        problem(unpack_zip(b"not a zip", &dir.join("e"))),
        Problem::Archive
    );
}

#[test]
fn a_zip_that_inflates_past_the_limits_is_refused() {
    let dir = TempDir::new("agentd-skill");
    let big = vec![0u8; usize::try_from(MAX_SKILL_BYTES).unwrap() + 1];
    let bytes = zip_of(&[("big", Some(&big), 0o100644)]);
    assert!(bytes.len() < 64 * 1024, "the archive itself is small");
    assert_eq!(
        problem(unpack_zip(&bytes, &dir.join("a"))),
        Problem::TooLarge
    );

    let names: Vec<String> = (0..=MAX_FILES).map(|i| format!("f{i}")).collect();
    let entries: Vec<(&str, Option<&[u8]>, u32)> = names
        .iter()
        .map(|name| (name.as_str(), Some(&b""[..]), 0o100644))
        .collect();
    assert_eq!(
        problem(unpack_zip(&zip_of(&entries), &dir.join("b"))),
        Problem::TooLarge
    );

    let deep = format!("{}f", "d/".repeat(MAX_DEPTH));
    assert_eq!(
        problem(unpack_zip(
            &zip_of(&[(&deep, Some(b"x"), 0o100644)]),
            &dir.join("c")
        )),
        Problem::TooLarge
    );

    let long = format!("{0}/{0}/{0}/{0}/{0}", "n".repeat(205));
    assert!(long.len() > MAX_PATH_BYTES);
    assert_eq!(
        problem(unpack_zip(
            &zip_of(&[(&long, Some(b"x"), 0o100644)]),
            &dir.join("d")
        )),
        Problem::TooLarge
    );
}

#[test]
fn a_tree_with_a_path_past_the_limit_is_refused() {
    let dir = TempDir::new("agentd-skill");
    let tree = dir.join("long");
    let part = "n".repeat(250);
    let fits = tree.join(format!("{part}/{part}/{part}/{part}"));
    fs::create_dir_all(&fits).unwrap();
    fs::write(tree.join("SKILL.md"), SKILL).unwrap();
    check_tree(&tree).unwrap();
    fs::write(fits.join("n".repeat(25)), "x").unwrap();
    assert_eq!(problem(check_tree(&tree)), Problem::TooLarge);
}

#[test]
fn a_zip_whose_declared_size_lies_is_refused() {
    let dir = TempDir::new("agentd-skill");
    let mut bytes = zip_of(&[("f", Some(b"hello world"), 0o100644)]);
    let central = bytes
        .windows(4)
        .rposition(|w| w == [0x50, 0x4b, 0x01, 0x02])
        .unwrap();
    bytes[central + 24..central + 28].copy_from_slice(&2u32.to_le_bytes());
    let local = 0;
    bytes[local + 22..local + 26].copy_from_slice(&2u32.to_le_bytes());
    assert!(matches!(
        unpack_zip(&bytes, &dir.join("a")),
        Err(CheckError::Problem(Problem::Archive | Problem::TooLarge))
    ));
}

#[test]
fn a_tree_with_a_symlink_special_file_or_hidden_name_is_refused() {
    let dir = TempDir::new("agentd-skill");
    let tree = dir.join("link");
    fs::create_dir_all(tree.join("a")).unwrap();
    fs::write(tree.join("SKILL.md"), SKILL).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", tree.join("a/passwd")).unwrap();
    assert_eq!(problem(check_tree(&tree)), Problem::Symlink);

    let tree = dir.join("fifo");
    fs::create_dir_all(&tree).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        tree.join("pipe"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::from_raw_mode(0o644),
        0,
    )
    .unwrap();
    assert_eq!(problem(check_tree(&tree)), Problem::SpecialFile);

    let tree = dir.join("hidden");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("a\u{200b}b"), "x").unwrap();
    assert_eq!(problem(check_tree(&tree)), Problem::BadName);

    let tree = dir.join("blank");
    fs::create_dir_all(tree.join(" ")).unwrap();
    fs::write(tree.join(" /a"), "x").unwrap();
    assert_eq!(problem(check_tree(&tree)), Problem::BadName);

    for (index, name) in ["a\u{fe0f}b", "a\u{3164}b"].into_iter().enumerate() {
        let tree = dir.join(format!("ignorable-{index}"));
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join(name), "x").unwrap();
        assert_eq!(problem(check_tree(&tree)), Problem::BadName, "{name:?}");
    }
}

#[test]
fn a_clones_git_directory_goes_and_modes_are_rewritten() {
    let dir = TempDir::new("agentd-skill");
    let tree = dir.join("clone");
    fs::create_dir_all(tree.join(".git/objects")).unwrap();
    fs::create_dir_all(tree.join("bin")).unwrap();
    fs::write(tree.join("SKILL.md"), SKILL).unwrap();
    fs::write(tree.join("bin/run"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(tree.join("bin/run"), fs::Permissions::from_mode(0o4775)).unwrap();
    fs::set_permissions(tree.join("SKILL.md"), fs::Permissions::from_mode(0o666)).unwrap();
    fs::set_permissions(tree.join("bin"), fs::Permissions::from_mode(0o777)).unwrap();
    check_tree(&tree).unwrap();
    assert!(!tree.join(".git").exists());
    assert_eq!(mode(&tree.join("bin/run")), 0o755);
    assert_eq!(mode(&tree.join("SKILL.md")), 0o644);
    assert_eq!(mode(&tree.join("bin")), 0o755);

    let tree = dir.join("big");
    fs::create_dir_all(&tree).unwrap();
    fs::File::create(tree.join("f"))
        .unwrap()
        .set_len(MAX_SKILL_BYTES + 1)
        .unwrap();
    assert_eq!(problem(check_tree(&tree)), Problem::TooLarge);
}

#[test]
fn the_skill_is_at_the_top_or_in_the_only_directory() {
    let dir = TempDir::new("agentd-skill");
    let top = dir.join("top");
    fs::create_dir_all(top.join("docs")).unwrap();
    fs::write(top.join("SKILL.md"), SKILL).unwrap();
    assert_eq!(find_skill(&top).unwrap().0, top);

    let nested = dir.join("nested");
    fs::create_dir_all(nested.join("pdf")).unwrap();
    fs::write(nested.join("pdf/SKILL.md"), SKILL).unwrap();
    assert_eq!(find_skill(&nested).unwrap().0, nested.join("pdf"));

    let two = dir.join("two");
    fs::create_dir_all(two.join("a")).unwrap();
    fs::create_dir_all(two.join("b")).unwrap();
    fs::write(two.join("a/SKILL.md"), SKILL).unwrap();
    assert_eq!(problem(find_skill(&two)), Problem::NoSkillFile);

    let empty = dir.join("empty");
    fs::create_dir_all(&empty).unwrap();
    assert_eq!(problem(find_skill(&empty)), Problem::NoSkillFile);

    let lone = dir.join("lone");
    fs::create_dir_all(&lone).unwrap();
    fs::write(lone.join("README.md"), SKILL).unwrap();
    assert_eq!(problem(find_skill(&lone)), Problem::NoSkillFile);

    let binary = dir.join("binary");
    fs::create_dir_all(&binary).unwrap();
    fs::write(binary.join("SKILL.md"), [0xff, 0xfe, 0x00]).unwrap();
    assert_eq!(problem(find_skill(&binary)), Problem::SkillFile);

    let large = dir.join("large");
    fs::create_dir_all(&large).unwrap();
    fs::File::create(large.join("SKILL.md"))
        .unwrap()
        .set_len(MAX_SKILL_MD_BYTES + 1)
        .unwrap();
    assert_eq!(problem(find_skill(&large)), Problem::SkillFile);
}

#[test]
fn an_uploaded_skill_file_is_capped() {
    let dir = TempDir::new("agentd-skill");
    write_skill_file(SKILL.as_bytes(), &dir.join("ok")).unwrap();
    assert_eq!(fs::read_to_string(dir.join("ok/SKILL.md")).unwrap(), SKILL);
    let big = vec![b'x'; usize::try_from(MAX_SKILL_MD_BYTES).unwrap() + 1];
    assert_eq!(
        problem(write_skill_file(&big, &dir.join("big"))),
        Problem::SkillFile
    );
}

#[test]
fn problems_never_repeat_the_content() {
    let secret = "SECRET-0f3a";
    for text in [
        format!("---\nname: {secret}\ndescription: y\n---\n"),
        format!("---\nname: x\ndescription: y\nallowed-hosts: [{secret}]\n---\n"),
        format!("---\nname: [{secret}\n---\n"),
    ] {
        let err = parse_skill_file(&text).unwrap_err();
        assert!(!err.to_string().contains(secret), "{err}");
    }
}
