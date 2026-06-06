use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::{BlockingSender, Connection};
use dbus::message::MatchRule;
use dbus::{Message, Path};
use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::{Mode, SFlag, fstat};
use nix::unistd::{Gid, Group, Uid, User, fchown, getgrouplist};
use std::collections::VecDeque;
use std::ffi::CString;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
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
    call_message.set_no_reply(false);
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
    call_message.set_no_reply(false);
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
    let gid = session_owner_is_in_users_group(uid)?;
    println!("Switching to {uid}:{gid}");
    let count = perform_chown(uid, gid);
    Some(count)
}

fn handle_seat_change<'a>(_signal: (), conn: &Connection, msg: &Message) -> bool {
    match potentially_change_ownership(conn, msg) {
        Some(count) => {
            println!("Updated ownership to {count} files");
        }
        _ => {
            // nothing to do
        }
    }

    true
}

fn perform_chown(uid: Uid, gid: Gid) -> u64 {
    let mut count = 0u64;
    let start_path = PathBuf::from("/opt/steamlib/");
    let expected_device = start_path
        .metadata()
        .expect("/opt/steamlib must exist")
        .dev();

    let mut stack = vec![start_path];

    while let Some(path) = stack.pop() {
        match process_next_file(&path, &mut stack, expected_device, uid, gid) {
            Ok(n) => {
                count += n;
            }
            Err(Errno::EMLINK) | Err(Errno::ELOOP) => {
                // nothing required, just a symlink we ignore
            }
            Err(errno) => {
                eprintln!(
                    "Could not change ownership of {} to {errno}",
                    path.display()
                );
            }
        }
    }

    count
}

fn process_next_file(
    path: &PathBuf,
    queue: &mut Vec<PathBuf>,
    expected_device: u64,
    user: Uid,
    group: Gid,
) -> nix::Result<u64> {
    let mut n = 0;
    let fd = open(path, OFlag::O_RDONLY | OFlag::O_NOFOLLOW, Mode::empty())?;
    // nofollow already removes all symlinks, no explicit check required
    let file_stat = fstat(&fd)?;
    if file_stat.st_dev != expected_device {
        eprintln!(
            "File {} device {} not equal to root device ({}). Suspicious, will not proceed",
            path.display(),
            file_stat.st_dev,
            expected_device
        );
    } else {
        let flags = SFlag::from_bits_truncate(file_stat.st_mode);

        if file_stat.st_uid != user.as_raw() || file_stat.st_gid != group.as_raw() {
            fchown(&fd, Some(user), Some(group))?;
            n = 1;
        }

        if flags.contains(SFlag::S_IFDIR) {
            let dir = Dir::from_fd(fd)?;

            for entry in dir {
                let entry = entry?;
                let mut next_path = path.clone();
                let name = entry.file_name();

                if c"." == name || c".." == name {
                    continue;
                }

                let Ok(name) = name.to_str() else {
                    eprintln!("UTF-8 fuckery on {name:?}, will not proceed");
                    continue;
                };
                next_path.push(name);
                queue.push(next_path);
            }
        }
    }
    Ok(n)
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
fn session_owner_is_in_users_group(uid: Uid) -> Option<Gid> {
    if let Ok(list) = get_group_list_for_user(uid) {
        list.iter().find(|g| g.name == "users").map(|g| g.gid)
    } else {
        None
    }
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
