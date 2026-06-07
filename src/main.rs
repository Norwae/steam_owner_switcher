use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::{BlockingSender, Connection};
use dbus::message::MatchRule;
use dbus::{Message, Path};
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{OFlag, OpenHow, ResolveFlag, open, openat2};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat};
use nix::unistd::{Gid, Group, Uid, User, fchown, getgrouplist};
use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

fn detect_switch_to_session(msg: &Message) -> Option<Path<'_>> {
    let (_if, dict, _inval) = Message::get3::<&str, PropMap, Vec<&str>>(msg);
    let dict = dict?;
    let session_value = dict.get_key_value("ActiveSession")?;

    let mut iter = session_value.1.0.as_iter()?;
    iter.next()?; // skip session id

    Some(iter.next()?.as_str()?.to_owned().into())
}

fn examine_session_for_user(connection: &Connection, session_path: Path) -> Option<u32> {
    let mut call_message = Message::call_with_args(
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

    call_message = Message::call_with_args(
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

fn potentially_change_ownership(conn: &Connection, msg: &Message) -> Option<u64> {
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
    perform_chown(uid, gid)
        .inspect_err(|err| {
            eprintln!(
                "Failed to initiate filesystem walk, /opt/steamlib absent or symlink?: {err}"
            );
        })
        .ok()
}

fn handle_seat_change<'a>(_signal: (), conn: &Connection, msg: &Message) -> bool {
    potentially_change_ownership(conn, msg)
        .inspect(|count| println!("Successfully changed {count} file owners"));

    true
}

#[derive(Debug)]
enum TraversalElement {
    Root(OwnedFd, FileStat),
    PendingChildNode(Rc<Dir>, CString),
}

impl TraversalElement {
    fn resolve(self) -> nix::Result<(OwnedFd, FileStat)> {
        match self {
            TraversalElement::Root(fd, stat) => Ok((fd, stat)),
            TraversalElement::PendingChildNode(parent, name) => {
                let fd = openat2(
                    parent,
                    name.as_c_str(),
                    OpenHow::new().flags(OFlag::O_RDONLY).resolve(
                        ResolveFlag::RESOLVE_NO_SYMLINKS
                            | ResolveFlag::RESOLVE_NO_XDEV
                            | ResolveFlag::RESOLVE_BENEATH
                            | ResolveFlag::RESOLVE_NO_MAGICLINKS,
                    ),
                )?;
                let stat = fstat(&fd)?;
                Ok((fd, stat))
            }
        }
    }
}

fn perform_chown(uid: Uid, gid: Gid) -> Result<u64, Errno> {
    let mut count = 0u64;
    let start_path = PathBuf::from("/opt/steamlib/");

    let fd = open(
        &start_path,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )?;
    let stat = fstat(&fd)?;
    let mut stack = vec![TraversalElement::Root(fd, stat)];

    while let Some(element) = stack.pop() {
        match process_next_file(element, &mut stack, &mut count, uid, gid) {
            Ok(_) | Err(Errno::ELOOP) | Err(Errno::ENOENT) | Err(Errno::EXDEV) => {
                // Success(ish) cases - we changed the file, it's gone, or we can't
            }
            Err(e) => {
                eprintln!("Error in tree walk: {e}");
            }
        }
    }

    Ok(count)
}

fn process_next_file(
    next: TraversalElement,
    queue: &mut Vec<TraversalElement>,
    count: &mut u64,
    user: Uid,
    group: Gid,
) -> nix::Result<()> {
    let (fd, file_stat) = next.resolve()?;

    if file_stat.st_uid != user.as_raw() || file_stat.st_gid != group.as_raw() {
        fchown(&fd, Some(user), Some(group))?;
        *count += 1;
    }

    if SFlag::from_bits_truncate(file_stat.st_mode).contains(SFlag::S_IFDIR) {
        let mut dir = Dir::from_fd(fd)?;
        let entries = dir.iter().collect::<Vec<_>>();
        let dir = Rc::new(dir);

        for entry in entries {
            let entry = entry?; // if directory walk fails for one entry, we just skip the rest
            if c"." == entry.file_name() || c".." == entry.file_name() {
                continue;
            }

            queue.push(TraversalElement::PendingChildNode(
                dir.clone(),
                entry.file_name().to_owned(),
            ));
        }
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

fn main() {
    let connection = Connection::new_system().expect("need D-Bus connection to work");
    let mut on_login =
        MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
    on_login.path = Some("/org/freedesktop/login1/seat/seat0".into());
    on_login.sender = Some("org.freedesktop.login1".into());

    connection
        .add_match(on_login, handle_seat_change)
        .expect("need to be able to subscribe to seat event");

    loop {
        connection
            .process(Duration::from_hours(24))
            .expect("DBus went away, can't continue");
    }
}
