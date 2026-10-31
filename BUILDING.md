# building

the standard build command is this:

    cargo install --locked --git https://github.com/x0rro/xara xara

the rest of the doc covers platform-specific deps and setup quirks.

## linux

on debian/ubuntu:

    sudo apt install libpulse-dev libasound2-dev

on fedora:

    sudo dnf install pulseaudio-lib-devel alsa-lib-devel

then build the usual way. the README covers all the main options for
linux.

## macOS

no additional deps. audio goes through CoreAudio. devices are named by
CoreAudio UID or exact name:

    xara --list-devices
    xara --call YOURCALL --hf 2300 \
         --capture "USB Audio CODEC" --playback "USB Audio CODEC" \
         --radio --ptt rts:/dev/cu.usbserial-0001

without `--radio`, playback only goes into a virtual device such as
[BlackHole](https://github.com/ExistentialAudio/BlackHole).

you will need to grant `xara` microphone access on the first run.

### ptt

serial ports are `/dev/cu.*`. cm108 PTT only works by USB id, e.g.,
`cm108:1209:7388` for an AIOC.

to start xara at login, copy the launchd agent, replace YOU in it with your
login name, then load it:

    cp dist/launchd/xara.plist ~/Library/LaunchAgents/
    launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/xara.plist

to stop it (a plain kill gets it restarted):

    launchctl bootout gui/$(id -u)/io.github.x0rro.xara

it will ask for microphone access on first launch and on every reinstall.

## freebsd

install the deps:

    pkg install rust pkgconf alsa-lib alsa-plugins pulseaudio

then build the usual way. pulse and alsa are both supported. to use
`--radio` with alsa, pin a PCM in `~/.asoundrc`:

    pcm.digirig {
        type plug
        slave.pcm { type oss device "/dev/dsp5" }
    }

then use `--audio alsa --capture digirig --playback digirig`.

### ptt

cm108 PTT needs `hidraw(4)`, not `uhid(4)`: put `hidraw_load="YES"` and
`hw.usb.usbhid.enable="1"` in `/boot/loader.conf`. normally root-only,
to grant the `operator` group access: put the following in
`/etc/devfs.rules` and set `devfs_system_ruleset="xara"` in
`/etc/rc.conf`.

    [xara=10]
    add path 'hidraw*' mode 0660 group operator

serial PTT uses the `/dev/cuaU*` devices.
