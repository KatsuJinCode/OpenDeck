# Place Action CLI

`--place-action` lets an external tool place an action in a live OpenDeck profile without editing profile JSON.

```bash
/usr/bin/opendeck --place-action <device> <profile> <controller> <position> <action-uuid> [--replace]
```

Example:

```bash
/usr/bin/opendeck --place-action sd-A00WA5211NLASR Default Encoder 0 com.jw.ai-usage.provider-usage --replace
```

## Behavior

- Sends the command to the already-running OpenDeck instance through Tauri single-instance handling.
- Looks up `<action-uuid>` in the registered action catalog.
- Verifies the action supports the destination controller.
- If `--replace` is present, removes the existing slot first.
- Calls the same `create_instance` path used by UI drag-and-drop and paste.
- Saves the profile through OpenDeck's profile store.
- Lazy-spawns the plugin if it was metadata-only.
- Sends `willAppear` for the new instance.

## Why This Exists

Before this, the only normal live placement paths were the GUI drag/drop and paste paths in `DeviceView.svelte`. External automation had no equivalent command, which pushed agents toward direct profile JSON edits. That is the wrong path for normal placement because the running app owns in-memory profile state and can overwrite direct file edits later.

`--process-message` is not a replacement. It feeds plugin WebSocket events into OpenDeck. Action placement is a Tauri frontend command, not a plugin event.

## Restart Rule

Placing an action with this command does not require restarting OpenDeck. A restart is only needed after installing a new OpenDeck binary, because the old running process cannot contain newly compiled code.
