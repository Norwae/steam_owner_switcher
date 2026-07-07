# How to enjoy

* Have another geek in your household
* Make a steam library in /opt/steamlib
* Have the other geek do the same(!)
* Build the project
* Install the binary to /usr/local/sbin/steamlib_owner_switch_daemon
* Install the unit
* Reload and enable

# How it works

Magic (actually D-Bus listening on who has your primary seat right now. If you have multiple, well... you're on your
own. That would take fancy VFS/FUSE stuff I don't want to deal with)

# But it doesn't work!?

Check a few things:
* /opt/steamlib exists? If you want it somewhere else, set STEAM_LIBRARY_ROOT in the unit file
* Is your co-nerd in the users group? Add them. If you want a dedicated group, you can set STEAM_GROUP_NAME in the unit file
* Does anything crash (`systemctl status`)? If so, give that a good luck and submit a PR.
