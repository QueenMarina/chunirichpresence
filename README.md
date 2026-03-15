# ChuniRichPresence

ChuniRichPresence is a Discord Rich Presence connector for Chunithm.
It is built as an injectable DLL that you can add to your `launch.bat`

## Preview

<p>
  <img src="images/song_select.png" height="320" />
  <img src="images/now_playing.png" height="320" />
</p>

## Configuration (segatools.ini)

You can customize behavior in `segatools.ini` under the `[chunirichpresence]` section.

```ini
[chunirichpresence]
game_name=Chunithm
logo_url=https://chunithm.org/assets/logo.png
discord_app_id=1482780703128289493
```

Supported options:

- `game_name`: display name used in Discord Rich Presence.
- `logo_url`: default image used when not actively playing a song.
- `discord_app_id`: Discord application ID used for RPC.

If a value is missing, the built-in default is used.
You do not need to create a Discord app ID, you can use the provided one, but the option is provided if needed

## Build Instructions (Rust)

1. Install Rust using rustup:
   - https://rustup.rs
2. Add the Windows GNU 32-bit target:
   - `rustup target add i686-pc-windows-gnu`
3. Build release DLL:
   - `cargo build --target i686-pc-windows-gnu --release`

Build output:

- `target/i686-pc-windows-gnu/release/chunirichpresence.dll`

## How to add

1. Copy `chunirichpresence.dll` to your game/launcher directory.
2. Edit your `launch.bat`.
3. Add this DLL to your injector command (same place you add other injected mods).

Example with `inject.exe`:

```bat
inject_x86 -d -k chusanhook_x86.dll -k chunirichpresence.dll chusanApp.exe
```

If your `launch.bat` already injects DLLs, append `chunirichpresence.dll` to that existing list.

## How-to on Linux

If you play Chunithm on Linux using wine, it will not work by default because wine has no way to connect to Discord Rich Presence.

You can use a bridge and run it in the same wineprefix as the game https://github.com/EnderIce2/rpc-bridge

This only works with the official version of Discord, Vesktop uses `arrpc` and it is not supported yet

## Testing

This has only been tested in Chunithm XVERSEX running under wine in Linux, using this [rpc bridge](https://github.com/EnderIce2/rpc-bridge) and the official Discord desktop app
