use nix::NixPath;
use nix::fcntl::{OFlag, OpenHow, ResolveFlag, open, openat2};
use nix::sys::stat::{Mode, SFlag, fstat};
use std::env::var;
use std::ffi::CString;
use std::os::fd::{AsFd, OwnedFd};
use std::rc::Rc;
use std::sync::Arc;

use lazy_static::lazy_static;
use nix::dir::{Dir, Entry};
use nix::errno::Errno;
use nix::unistd::{Gid, Group, Uid, User, dup, fchown, getgrouplist};
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio::task::spawn_blocking;
use zbus::Connection;
use zbus::export::ordered_stream::OrderedStreamExt;
use zbus::fdo::{PropertiesChangedArgs, PropertiesProxy};
use zbus::names::InterfaceName;
use zbus::zvariant::{Structure, Value};

const LIBRARY_MARKER_PATH: &str = "libraryfolder.vdf";
lazy_static! {
    static ref SESSION: InterfaceName<'static> =
        "org.freedesktop.login1.Session".try_into().unwrap();
    static ref STEAM_GROUP_NAME: String = var("STEAM_GROUP_NAME").unwrap_or("users".to_string());
}

fn verify_and_perform_change(root: OwnedFd, uid_to_switch: u32) -> Option<u64> {
    let uid = Uid::from_raw(uid_to_switch);
    let Some(gid) = get_legitimizing_group_from(uid) else {
        println!("User {uid} not in the legitimizing group");
        return None;
    };

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

fn get_legitimizing_group_from(uid: Uid) -> Option<Gid> {
    let Ok(Some(user)) = User::from_uid(uid) else {
        return None;
    };

    let Ok(user_name) = CString::new(user.name) else {
        eprintln!("Username had embedded nul character(s), {uid} is not a valid switch target");
        return None;
    };

    for gid in getgrouplist(user_name.as_c_str(), user.gid).ok()? {
        if let Ok(Some(group)) = Group::from_gid(gid) {
            if STEAM_GROUP_NAME.as_str() == group.name {
                return Some(gid);
            }
        }
    }
    None
}
fn sanity_check() -> OwnedFd {
    let root = var("STEAM_LIBRARY_ROOT").unwrap_or_else(|_| "/opt/steamlib".to_owned());
    println!("Validating root directory '{root}'");

    let root = open(
        root.as_str(),
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .expect("Root exists and non-symlink dir");

    safe_open(&root, LIBRARY_MARKER_PATH).expect("Library root file present");

    root
}

async fn handle_one_seat_change(
    connection: &Connection,
    args: zbus::Result<PropertiesChangedArgs<'_>>,
) -> zbus::Result<Option<u32>> {
    let args = args?; // to make argument resolution hit the same error handler
    if "org.freedesktop.login1.Seat" != args.interface_name.as_str() {
        return Ok(None);
    }

    let Some(session) = args.changed_properties.get("ActiveSession") else {
        return Ok(None);
    };

    let session: &Structure = session.downcast_ref()?;
    Ok(attempt_resolve_session_owner_uid(connection, session).await)
}

async fn attempt_resolve_session_owner_uid(
    connection: &Connection,
    carrier: &Structure<'_>,
) -> Option<u32> {
    if carrier.fields().len() == 2
        && let Value::ObjectPath(path) = &carrier.fields()[1]
        && path != "/"
    {
        // if anything is fishy or unexpected, we do not hard-error but stop processing
        let proxy = PropertiesProxy::new(connection, "org.freedesktop.login1", path)
            .await
            .ok()?;
        let response = proxy.get(SESSION.clone(), "User").await.ok()?;
        let response: &Structure = response.downcast_ref().ok()?;
        let uid: &u32 = response.fields()[0].downcast_ref().ok()?;
        Some(*uid)
    } else {
        None
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> zbus::Result<()> {
    let root = sanity_check();
    let walk_mutex = Arc::new(Mutex::new(root));
    let connection = Connection::system().await?;
    let proxy = PropertiesProxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1/seat/seat0",
    )
    .await?;
    let mut stream = proxy.receive_properties_changed().await?;
    while let Some(seat_change_event) = stream.next().await {
        match handle_one_seat_change(&connection, seat_change_event.args()).await {
            Ok(None) => {
                // irrelevant / malformed change
            }
            Ok(Some(uid)) => {
                let walk_mutex = walk_mutex.clone();
                spawn_blocking(move || {
                    let root = Handle::current().block_on(walk_mutex.lock());
                    let root = dup(root.as_fd()).expect("FD table exhausted");
                    println!("Seat transferred to {uid}");
                    if let Some(count) = verify_and_perform_change(root, uid) {
                        println!("Updated {count} file ownership markers");
                    }
                });
            }
            Err(e) => eprintln!("Error handling event {seat_change_event:?}: {e}"),
        };
    }

    panic!("The seat change stream ended. Crash to get respawned after sanity returns")
}
