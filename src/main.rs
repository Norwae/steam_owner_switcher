use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::{BlockingSender, Connection};
use dbus::message::MatchRule;
use dbus::{Message, Path};
use nix::unistd::{Group, Uid, User, getgrouplist};
use std::collections::VecDeque;
use std::ffi::CString;
use std::fs::{read_dir, symlink_metadata};
use std::os::unix::fs::{MetadataExt, lchown};
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

fn do_ownership_switch<'a>(_signal: (), conn: &Connection, msg: &Message) -> bool {
    let session = detect_switch_to_session(msg);
    if let Some(session) = session {
        println!("Seat changed to {session}");
        if let Some(uid_to_switch) = examine_session_for_user(conn, session) {
            println!("Session owned by {uid_to_switch}, checking users group");
            let uid_to_switch = Uid::from_raw(uid_to_switch);
            if session_owner_is_in_users_group(uid_to_switch) {
                println!("Switching to {uid_to_switch}");
                let count = perform_chown_to_uid(uid_to_switch);
                println!("Switch complete, altered {count} files");
            }
        }
    }
    true
}

fn perform_chown_to_uid(uid: Uid) -> u64 {
    let path = PathBuf::from("/opt/steamlib/");
    let meta = path.symlink_metadata().unwrap();
    // safe because we just matched the user to be in this group
    let group = Group::from_name("users")
        .expect("group lookup")
        .expect("users group exists")
        .gid;

    walk_and_chown(path, meta.dev(), uid.as_raw(), group.as_raw())
}

fn chown_and_queue_children(
    path: &PathBuf,
    queue: &mut VecDeque<PathBuf>,
    count: &mut u64,
    dev: u64,
    uid: u32,
    gid: u32,
) -> Result<(), std::io::Error> {
    let path_metadata = symlink_metadata(path)?;
    if !path_metadata.is_symlink() {
        let this_device = path_metadata.dev();
        if this_device != dev {
            eprintln!(
                "Crossing device boundary from {dev} to {this_device}, dubious, we're not doing this"
            )
        } else {
            if path_metadata.uid() != uid || path_metadata.gid() != gid {
                lchown(path, Some(uid), Some(gid))?;
                *count += 1;
            }
            if path_metadata.is_dir() {
                for child in read_dir(path)? {
                    let child = child?;
                    queue.push_back(child.path());
                }
            }
        }
    }
    Ok(())
}

fn walk_and_chown(path: PathBuf, dev: u64, uid: u32, gid: u32) -> u64 {
    let mut queue = VecDeque::new();
    let mut count = 0;
    queue.push_back(path);

    while let Some(path) = queue.pop_front() {
        let error = chown_and_queue_children(&path, &mut queue, &mut count, dev, uid, gid).err();
        if let Some(error) = error {
            eprintln!("Could not introspect and chown at {path:?}: {error}")
        }
    }

    count
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
fn session_owner_is_in_users_group(uid: Uid) -> bool {
    if let Ok(list) = get_group_list_for_user(uid) {
        list.iter().any(|g| g.name == "users")
    } else {
        false
    }
}

fn main() {
    let connection = Connection::new_system().expect("need D-Bus connection to work");
    let mut on_login =
        MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
    on_login.path = Some("/org/freedesktop/login1/seat/seat0".into());
    on_login.sender = Some("org.freedesktop.login1".into());

    connection
        .add_match(on_login, do_ownership_switch)
        .expect("need to be able to subscribe to seat event");

    loop {
        connection
            .process(Duration::from_hours(24))
            .expect("DBus went away, can't continue");
    }
}
