use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::{BlockingSender, Connection};
use dbus::message::MatchRule;
use dbus::{Message, Path};
use nix::NixPath;
use nix::dir::{Dir, Entry};
use nix::errno::Errno;
use nix::fcntl::{OFlag, OpenHow, ResolveFlag, open, openat2};
use nix::sys::stat::{Mode, SFlag, fstat};
use nix::unistd::{Gid, Group, Uid, User, dup, fchown, getgrouplist};
use std::env::var;
use std::ffi::CString;
use std::os::fd::{AsFd, OwnedFd};
use std::rc::Rc;
use std::time::Duration;

const LIBRARY_MARKER_PATH: &str = "libraryfolder.vdf";

fn detect_switch_to_session(msg: &Message) -> Option<Path<'_>> {
    let (_if, dict, _inval) = Message::get3::<&str, PropMap, Vec<&str>>(msg);
    let dict = dict?;
    let session_value = dict.get_key_value("ActiveSession")?;

    let mut iter = session_value.1.0.as_iter()?;
    iter.next()?; // skip session id

    Some(iter.next()?.as_str()?.to_owned().into())
}

fn examine_session_for_user(connection: &Connection, session_path: Path) -> Option<u32> {
    let call_message = Message::call_with_args(
        "org.freedesktop.login1",
        session_path,
        "org.freedesktop.DBus.Properties",
        "Get",
        ("org.freedesktop.login1.Session", "User"),
    );
    let response = connection
        .send_with_reply_and_block(call_message, Duration::from_millis(100))
        .ok()?;
    let user_path: Variant<(u32, Path)> = response.get1()?;

    let call_message = Message::call_with_args(
        "org.freedesktop.login1",
        user_path.0.1,
        "org.freedesktop.DBus.Properties",
        "Get",
        ("org.freedesktop.login1.User", "UID"),
    );
    let response = connection
        .send_with_reply_and_block(call_message, Duration::from_millis(100))
        .ok()?;

    let uid_response: Variant<u32> = response.get1()?;

    Some(uid_response.0)
}

fn potentially_change_ownership(root: OwnedFd, conn: &Connection, msg: &Message) -> Option<u64> {
    let session = detect_switch_to_session(msg)?;
    println!("Seat changed to {session}");
    let uid_to_switch = examine_session_for_user(conn, session)?;
    println!("Session owned by {uid_to_switch}, checking users group");
    let uid = Uid::from_raw(uid_to_switch);
    let gid = get_group_list_for_user(uid)
        .ok()?
        .into_iter()
        .find(|g| g.name == "users")?
        .gid;

    println!("Switching to {uid}:{gid}");
    perform_chown(root, uid, gid)
        .inspect_err(|err| {
            eprintln!("Failed to initiate filesystem walk, root path absent or symlink?: {err}");
        })
        .ok()
}

struct ToChown(Rc<Dir>, Entry);
fn enqueue_dir_content(dir_fd: OwnedFd, to: &mut Vec<ToChown>) -> nix::Result<()> {
    let mut dir = Dir::from_fd(dir_fd)?;
    // using iter ensures the dir fd is correctly rewound
    let entries = dir.iter().collect::<Vec<_>>();
    let dir = Rc::new(dir);
    for entry in entries {
        let entry = entry?; // if directory walk fails for one entry, we just skip the rest
        if c"." == entry.file_name() || c".." == entry.file_name() {
            continue;
        }

        to.push(ToChown(dir.clone(), entry));
    }
    Ok(())
}

fn perform_chown(root: OwnedFd, uid: Uid, gid: Gid) -> Result<u64, Errno> {
    let marker_fd = safe_open(&root, LIBRARY_MARKER_PATH)?;
    let marker = fstat(&marker_fd)?;

    if marker.st_uid == uid.as_raw() && marker.st_gid == gid.as_raw() {
        println!("Library file belongs to current user, no deep scan.");
        return Ok(0);
    }
    drop(marker_fd);

    let mut count = 0u64;
    let mut stack = Vec::new();
    enqueue_dir_content(root, &mut stack)?;

    while let Some(element) = stack.pop() {
        match process_next_file(element, &mut stack, uid, gid) {
            Ok(_) => count += 1,
            Err(Errno::ELOOP) | Err(Errno::ENOENT) | Err(Errno::EXDEV) => {
                // Success(ish) cases - we changed the file, it's gone, or we can't
            }
            Err(e) => {
                eprintln!("Error in tree walk: {e}");
            }
        }
    }

    Ok(count)
}

fn safe_open<FD: AsFd, P: ?Sized + NixPath>(dir: FD, path: &P) -> nix::Result<OwnedFd> {
    openat2(
        dir,
        path,
        OpenHow::new().flags(OFlag::O_RDONLY).resolve(
            ResolveFlag::RESOLVE_NO_SYMLINKS
                | ResolveFlag::RESOLVE_NO_XDEV
                | ResolveFlag::RESOLVE_BENEATH
                | ResolveFlag::RESOLVE_NO_MAGICLINKS,
        ),
    )
}

fn process_next_file(
    next: ToChown,
    queue: &mut Vec<ToChown>,
    user: Uid,
    group: Gid,
) -> nix::Result<()> {
    let ToChown(dir, entry) = next;
    let fd = safe_open(dir, entry.file_name())?;
    fchown(&fd, Some(user), Some(group))?;
    if SFlag::from_bits_truncate(fstat(&fd)?.st_mode).contains(SFlag::S_IFDIR) {
        enqueue_dir_content(fd, queue)?;
    }

    Ok(())
}

fn get_group_list_for_user(uid: Uid) -> nix::Result<Vec<Group>> {
    let Some(user) = User::from_uid(uid)? else {
        return Ok(vec![]);
    };

    let Ok(user_name) = CString::new(user.name) else {
        eprintln!("Username had embedded nul character(s), {uid} is not a valid switch target");
        return Ok(vec![]);
    };

    let mut collected = vec![];
    for gid in getgrouplist(user_name.as_c_str(), user.gid)? {
        if let Some(group) = Group::from_gid(gid)? {
            collected.push(group);
        }
    }

    Ok(collected)
}

fn sanity_check() -> OwnedFd {
    let root = var("STEAM_LIBRARY_ROOT").unwrap_or_else(|_| "/opt/steamlib".to_owned());
    let root: &str = &root;

    let root_fd = open(
        root,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .expect("Root exists and non-symlink dir");

    safe_open(&root_fd, LIBRARY_MARKER_PATH).expect("Library root file present");

    root_fd
}

fn main() {
    let root = sanity_check();
    let connection = Connection::new_system().expect("need D-Bus connection to work");
    let mut on_login =
        MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
    on_login.path = Some("/org/freedesktop/login1/seat/seat0".into());
    on_login.sender = Some("org.freedesktop.login1".into());

    connection
        .add_match(on_login, move |_: (), conn: &Connection, msg: &Message| {
            if let Ok(root_copy) = dup(&root) {
                potentially_change_ownership(root_copy, conn, msg)
                    .inspect(|count| println!("Successfully changed {count} file owners"));
            } else {
                eprintln!("Could not duplicate root descriptor");
            }
            true
        })
        .expect("need to be able to subscribe to seat event");

    loop {
        connection
            .process(Duration::from_hours(24))
            .expect("DBus went away, can't continue");
    }
}
