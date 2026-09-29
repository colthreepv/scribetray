# Development notes

This document collects local deployment and implementation details for
contributors. Public setup instructions are in the [README](../README.md).

## Build and deploy locally

Requirements are Rust 1.85 or newer and the Windows MSVC toolchain. The
PowerShell helper builds a locked release executable, stages it outside the
repository, and switches a stable <code>latest</code> junction to the new
version:

```powershell
.\scripts\deploy-latest.ps1
& "$env:LOCALAPPDATA\Scribetray\builds\latest\scribetray.exe"
```

By default, builds are stored under
<code>%LOCALAPPDATA%\Scribetray\builds</code>, and the newest three are retained.
Running older builds are kept until they exit. Each build has a unique
versioned directory and a <code>build.json</code> record; the
<code>latest</code> junction points to the selected build.

The helper accepts <code>-BuildRoot</code>, <code>-KeepBuilds</code>,
<code>-Target</code>, and <code>-CargoArgs</code>. For example:

```powershell
.\scripts\deploy-latest.ps1 -BuildRoot 'D:\Apps\Scribetray\builds' -KeepBuilds 5
.\scripts\deploy-latest.ps1 -CargoArgs @('--jobs', '4')
```

Persistent defaults can be saved in
<code>%APPDATA%\Scribetray\deploy.json</code>:

```json
{
  "buildRoot": "D:\\Apps\\Scribetray\\builds",
  "keepBuilds": 5,
  "target": "x86_64-pc-windows-msvc",
  "cargoArgs": ["--jobs", "4"]
}
```

For the build root, retention count, and target, command-line values take
precedence over the <code>SCRIBETRAY_BUILD_ROOT</code>,
<code>SCRIBETRAY_KEEP_BUILDS</code>, and <code>SCRIBETRAY_TARGET</code>
environment variables; environment values take precedence over JSON settings.
Command-line Cargo arguments override <code>cargoArgs</code> in JSON. The
default retention count is three. The helper runs
<code>cargo build --release --locked</code> and promotes the output only after
the executable has been staged.

## Manual single-user release flow

Releases are prepared and tested locally by one user. Commit the complete
release source locally first, and build each candidate from a commit. Keep
Cargo and the executable at the intended stable release version (for example,
<code>0.6.3</code>). A test label such as <code>0.6.3-test.1</code> is a
candidate suffix, not a Cargo version or executable version.

Pass the candidate suffix to the deploy helper, for example:

```powershell
.\scripts\deploy-latest.ps1 -CandidateLabel '0.6.3-test.1'
```

The helper keeps the stable Cargo/executable version in the build and records
the suffix as <code>candidateLabel</code> in <code>build.json</code>. It copies
the candidate executable and metadata to
<code>dist\candidates\0.6.3-test.1\</code>, where test builds sharing the
same stable version remain distinguishable. Keep the selected candidate
available until it is rejected or packaged.

Close the old Scribetray instance completely, including its tray process, then
launch <code>dist\candidates\0.6.3-test.1\scribetray.exe</code>. Confirm its
<code>build.json</code> identifies the intended candidate and source commit,
then test it. If changes are needed, commit them locally and build a new
candidate with the next suffix. Do not push commits or create or push tags
before the user approves the tested candidate.

After approval, create an annotated stable tag (for example,
<code>v0.6.3</code>) on the approved source commit and push that commit and tag
to the configured Gitea repository. Verify that the one-way mirror makes the
same commit and tag available on GitHub. Gitea publishes source commits and
tags; the GitHub release publishes downloadable assets.

```powershell
git tag -a v0.6.3 -m "Scribetray v0.6.3"
git push origin main refs/tags/v0.6.3
```

After the mirror has the tag, prepare the package using the exact approved
candidate and an agent-written summary of the commits since the previous stable
tag. Review the generated notes and assets, then publish them:

```powershell
.\scripts\release.ps1 -CandidateLabel '0.6.3-test.1' -Summary 'One to three concise sentences describing the user-visible changes.'
.\scripts\release.ps1 -CandidateLabel '0.6.3-test.1' -Action Publish
```

The script checks that the candidate checksum matches, the stable tag points
to the candidate commit on local Git, Gitea, and GitHub, and Cargo still has the
stable version. It packages the exact candidate executable into
<code>scribetray-v0.6.3-x86_64.zip</code> and writes
<code>SHA256SUMS.txt</code>. It never creates or pushes tags. Use the
[release notes template](release-notes-template.md), which has only the
<code>{{VERSION}}</code> and <code>{{SUMMARY}}</code> placeholders. This flow
does not use CI to build or publish releases.

## Realtime transcription and fallback

Batch mode records audio locally and submits it to Scribe after recording
stops. Realtime mode opens an ElevenLabs Scribe v2 Realtime WebSocket when
recording starts and streams 16 kHz mono signed 16-bit PCM. Scribetray manually
commits segments about every 25 seconds, then joins the committed text when
recording ends.

If Scribetray cannot start the realtime connection, or the realtime session
fails, it falls back to sending the complete saved recording through the batch
<code>scribe_v2</code> endpoint. Optional <code>keyterms</code> are sent as
vocabulary hints in realtime mode and batch mode. The default is an empty list.
Realtime is disabled by default.

## Versioning

Versions follow implementation milestones. M0 was a disposable spike; M1 maps
to <code>v0.1.x</code>, M2 to <code>v0.2.x</code>, and M3 to
<code>v0.3.x</code>. Recovery and send features map to <code>v0.4.x</code>,
ElevenLabs usage to <code>v0.5.x</code>, and the usage header to
<code>v0.6.x</code>. Patch numbers increment for fixes within the current
milestone. The tray tooltip shows the running package version.

## Caret detection and text insertion

At recording start, Scribetray snapshots the foreground window and focused
control. It looks for a caret rectangle through Win32 GUI thread information,
Microsoft Active Accessibility (MSAA), and UI Automation text patterns. If an
app does not expose a usable caret, the mouse position is used as the overlay
anchor. The overlay refreshes while the original input target remains active.

Before inserting a transcript, Scribetray checks that the original foreground
window and focused control are still current. If they changed, it copies the
transcript to the clipboard rather than typing into another app. Type mode sends
Unicode keyboard input. Paste mode temporarily places the transcript on the
clipboard and attempts to restore the previous clipboard data after the paste;
if the clipboard changed in the meantime, Scribetray preserves the newer data.

Caret reporting and input behavior vary by app. Terminals, browser editors, and
other custom controls may expose different accessibility patterns, so verify
those targets when changing caret or insertion code.

## Start with Windows

The tray option writes Scribetray's executable path to the current user's
<code>HKCU\Software\Microsoft\Windows\CurrentVersion\Run</code> registry key.
The path is captured when the option is enabled. Keep that executable path
available while autostart is enabled; after moving Scribetray, disable and
enable the option again so Windows stores the new path. Scribetray requires an
interactive Windows desktop for its tray icon, microphone capture, hotkeys,
and text insertion.
