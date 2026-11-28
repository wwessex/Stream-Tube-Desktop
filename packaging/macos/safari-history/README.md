# Safari watch-history prototype

Build the prototype from the repository root on Apple Silicon with Xcode and the
[macOS build dependencies](../../../README.md#macos-local-build). Node.js is
needed to run the extension tests.

```bash
STREAMTUBE_SAFARI_PROTOTYPE=1 RUSTUP_TOOLCHAIN=1.100.0 bash packaging/macos/build-app.sh
node --test packaging/macos/safari-history/connect.test.cjs
```

The build produces a separately identified **Stream Tube Safari Prototype.app**
with an embedded Safari extension. Its default data directory is
`~/Library/Application Support/StreamTube Safari Prototype`. Set
`STREAMTUBE_SAFARI_SIGNING_IDENTITY` to a development signing identity to use it
for both bundles. With the default local ad hoc signing, Safari requires its
documented developer approval before it will load the extension.
[Running a Safari web extension](https://developer.apple.com/documentation/safariservices/running-your-safari-web-extension)

1. Open the prototype, then enable **Stream Tube Watch History** in Safari.
2. In Stream Tube, select **Connect** next to **YouTube watch history and
   comment reactions** under **Settings → General**.
   Choose **Use Safari** if the dialog also shows a Chromium browser.
3. In the YouTube tab, sign in and open **Stream Tube Watch History** from
   the Safari toolbar. Grant access to YouTube when Safari asks, then reopen the
   extension if needed and choose **Connect YouTube**.
4. Return to Stream Tube and check that Settings shows **Connected as**. Play a
   video in the native player with watch history enabled. Let it play for at
   least 30 seconds, then close the player and note the position.
5. In the same signed-in account, confirm the video appears in YouTube history.
   Open it from history and verify the position near where playback stopped.

The extension requests access to `www.youtube.com`. After you choose to connect,
it sends the YouTube session cookies from the active Safari profile to Stream
Tube over a one-time loopback connection tied to the open dialog. Stream Tube
uses one connection for playback reporting and comment reactions. It keeps the
session in memory until you disconnect or quit. Reconnect after restarting the
prototype. The device-code sign-in for recommendations and subscriptions is optional.

## Like or dislike comments

1. Open a video’s comments in Stream Tube. If the browser account is still
   disconnected, choose **Connect to react to comments** to open the same
   connection dialog as in Settings.
2. Choose **Reload comments** and use the comment’s like or dislike button.
   Click the selected button again to clear your reaction.
3. Choose **Reload comments** again to fetch the saved reaction from YouTube.

Voting and reloading happen in Stream Tube. Use **Disconnect** in **Settings →
General** to stop sharing the session for both watch history and reactions.

In the Apple Silicon 1.0.0 foreground test, the like and dislike each survived
**Reload comments** on Blender Studio’s [Sprite Fright](https://www.youtube.com/watch?v=_cMxraX_5RE).
The test reaction was cleared and a final reload confirmed the comment was unrated.

## Build a test copy alongside older prototypes

Set `STREAMTUBE_SAFARI_TEST_ID` to a label for this copy. Use 1–32 lowercase
ASCII letters, digits or hyphens, with a letter or digit at each end. The label
also enables Safari packaging.

```bash
STREAMTUBE_SAFARI_TEST_ID=resume-check RUSTUP_TOOLCHAIN=1.100.0 \
  bash packaging/macos/build-app.sh "$HOME/Downloads/StreamTube-builds"
```

Open the printed **Stream Tube Safari Test (resume-check).app** path and enable
**Stream Tube Watch History (resume-check)** in Safari for the profile you use
for YouTube. Start **Connect** in that app, then use the extension with the
same label on the YouTube tab and approve access when Safari asks.

Each label gets its own app and extension identifiers, URL scheme and data
directory. This example uses
`~/Library/Application Support/StreamTube Safari Test (resume-check)`. Use a
different label for each copy you want to keep available side by side. If you
rebuild with the same label, replace the previous copy with that label so
Safari has one app bundle for that extension identifier.

## Playback diagnostics

Quit the running prototype and launch it with diagnostics enabled. Replace the
app path with the path printed by the build script.

```bash
open -a "/path/to/Stream Tube Safari Prototype.app" \
  --env STREAMTUBE_WATCH_HISTORY_DIAGNOSTICS=1 --stderr /path/to/streamtube-watch.log
```

Lines beginning with `Watch history:` show the timing and state fields sent
in each playback and watch-time request, alongside the HTTP status. A report
with `final=1` marks the end of playback. `http=unavailable` means no HTTP
response was received. The diagnostic lines exclude request URLs, headers,
session identifiers and video IDs. HTTP success confirms receipt of the request.

To check shutdown reporting, repeat the playback test once using **Quit Stream
Tube** from the app menu and once by closing the main window. Compare the final
report's `cmt` field with the saved playback position. On normal exit, Stream
Tube gives queued reports up to three seconds to finish. Check the video directly
in YouTube History to verify the resume position there.

## Verified quit and resume

On Apple Silicon, the Safari prototype at [commit `1302fee`](https://github.com/wwessex/Stream-Tube-Desktop/commit/1302fee903c4aecc07076b6bf55645344118c709)
played [CHARGE](https://www.youtube.com/watch?v=UXqq0ZvbOnk) from the beginning.
**Quit Stream Tube** saved the position and sent the final watch-time report.

| Check | Observed result |
| --- | --- |
| Saved position on quit | 57.208 seconds |
| Final watch-time report | `cmt=57.208`, `final=1`, HTTP `204` |
| Opened directly from YouTube Watch History in Safari | YouTube opened the video with `t=57s`, and playback advanced from `0:57` to `0:59`. |

The preceding periodic report was at `0:40`. The History entry resumed at the
final quit position to the whole second.

## Verified main-window close and resume

On Apple Silicon, the Safari prototype built from the fork’s `main` at
[commit `6b88ee4`](https://github.com/wwessex/Stream-Tube-Desktop/commit/6b88ee46e9e6b84a2e575d18795cbb8bc9e8b3fd)
played [Coffee Run](https://www.youtube.com/watch?v=PVGeM40dABA) from the beginning.
Selecting **Close Window** from the main window’s native controls saved the
position, sent the final watch-time report and exited Stream Tube.

| Check | Observed result |
| --- | --- |
| Saved position when the main window closed | 35.041 seconds |
| Final watch-time report | `cmt=35.041`, `final=1`, HTTP `204` |
| Opened directly from YouTube Watch History in Safari | YouTube opened the video with `t=35s`, and playback advanced from `0:35` to `0:37`. |

The preceding periodic report was at `0:25`. The History entry resumed at the
final position to the whole second.

## Verified last-window close with `resume-check`

On Apple Silicon, the 0.7.5-alpha build packaged with
`STREAMTUBE_SAFARI_TEST_ID=resume-check` connected through **Stream Tube Watch
History (resume-check)** alongside the older Safari prototypes. The test played
*Sintel* from the beginning and closed the last window during playback.
[Sintel – Open Movie by Blender Foundation](https://www.youtube.com/watch?v=eRsGyueVLvQ)

| Check | Observed result |
| --- | --- |
| Position saved when the last window closed | 42.042 seconds |
| Final watch-time report | `cmt=42.042`, `final=1`, HTTP `204` |
| Opened directly from YouTube Watch History in Safari | Safari opened the video with `t=42s`. Playback resumed at `0:42` and advanced to `0:44`. |

## Verified last-window close from PR #243

On Apple Silicon, the opt-in build from
[PR #243](https://github.com/Avaneesh-Inamdar/Stream-Tube-Desktop/pull/243) at
commit `9975d9a` connected through **Stream Tube Watch History (pr243-resume-check)**.
The test played [Sprite Fright](https://www.youtube.com/watch?v=_cMxraX_5RE) from
the beginning and used **⌘W** to close the last Stream Tube window during playback.

| Check | Observed result |
| --- | --- |
| Position saved when the last window closed | 43.208 seconds, confirmed in Stream Tube’s local history |
| Final watch-time report | `cmt=43.208`, `final=1`, HTTP `204` |
| Opened directly from YouTube Watch History in Safari | Safari opened the video with `t=43s`. Playback resumed at `0:43` and advanced to `0:46`. |

The preceding periodic report was at `0:40`. Safari resumed at the final close position to the whole second.
