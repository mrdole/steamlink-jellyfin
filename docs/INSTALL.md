# Install

Early alpha: these are the steps that worked on one Link. Each one says how to check it.

## 1. SSH into the Steam Link

1. Format a USB stick FAT32 and create the empty file `steamlink/config/system/enable_ssh.txt` on it.
2. Plug it into the Link and power-cycle the Link.
3. `ssh root@<link-ip>`. The factory root password is `steamlink123`. Change it with `passwd`.
4. Install a key: append your public key to `/home/steam/.ssh/authorized_keys`, then
   `chmod go-w /home/steam` (if root's home is group-writable, sshd ignores the key).
5. Add a host alias on your computer, `~/.ssh/config`:

   ```
   Host steamlink
       HostName <link-ip>
       User root
       IdentityFile ~/.ssh/<your key>
   ```

   Check: `ssh steamlink uname -a` prints a 3.8 kernel. Give the Link a DHCP reservation so the address stays put.
   Another alias name works too: set `SLJF_HOST=<alias>` for every script below.

Also set the Link's **Settings -> Sleep after** to **Never**: idle sleep cuts HDMI and the network together.

## 2. Build (on a Linux x86-64 machine, no root)

```bash
git clone --depth 1 https://github.com/ValveSoftware/steamlink-sdk ~/steamlink-sdk   # or set SDK=/path
rustup target add armv7-unknown-linux-musleabihf
scripts/build.sh
scripts/build-app.sh
```

`build.sh` downloads mbedTLS and FFmpeg (versions and SHA-256 pinned in the script) into `$SDK/build-sljf/` and
builds a minimal static FFmpeg, the player (`out/sljf-play`) and the daemon (`out/sljfd`, static musl). It also copies
your machine's CA bundle (`out/ca.pem`, because the Link's own is from 2013) and Noto Sans Bold for subtitles.
`build-app.sh` builds the Qt app with the SDK's qmake and writes `out/app/jellyfin/`; it needs ImageMagick
(`magick`) for the tile icon and the theme's font installed (`fc-match "Noto Sans"`).

Check: `file out/sljfd` says `ARM, EABI5 ... statically linked`.

## 3. Log in to Jellyfin (once)

```bash
tools/sljf-login --server https://jellyfin.example.com --user <name>
```

It asks for the password, gets a token for a device called "Steam Link", and writes
`/mnt/scratch/sljf/config.json` (mode 600) on the Link over ssh's stdin. Nothing secret is stored on your computer.
A dedicated Jellyfin user for the Link is a good idea. If the account only allows listed devices, the login gets
HTTP 403: allow the new device in the Jellyfin dashboard and try again.

## 4. Deploy

```bash
scripts/deploy.sh
```

Copies everything to `/mnt/scratch/sljf/` and `/home/apps/jellyfin/`, installs the boot hook
`/etc/init.d/startup/S98sljfd`, and starts the daemon. Check: the script prints the daemon's log tail with
`connected`; the Jellyfin dashboard lists the device "Steam Link"; a "Jellyfin" tile is on the Link's home screen.

## Update

`scripts/build.sh && scripts/build-app.sh && scripts/deploy.sh`.

## Uninstall

```bash
ssh steamlink '/etc/init.d/startup/S98sljfd stop; rm -rf /etc/init.d/startup/S98sljfd /mnt/scratch/sljf /home/apps/jellyfin'
```

Then remove the "Steam Link" device in the Jellyfin dashboard (revokes the token).

## Troubleshooting

| Symptom | Look at |
|---|---|
| Nothing in Jellyfin's device list | `ssh steamlink tail -50 /mnt/scratch/sljf/sljfd.log`; right after a boot the clock is 1970 until NTP sets it and TLS fails for up to a minute |
| Picture but no sound, or sound drifts | `av_offset_ms` in config (CONFIG.md) |
| Playback fails while testing | Only one program may use the decoder: stop playback before running `tools/sljf-test` |
| Reboot | `ssh steamlink /etc/init.d/reboot.sh` (plain `reboot` does nothing on the Link) |
