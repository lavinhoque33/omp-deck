# OmpDeck privacy policy

OmpDeck is a desktop app that runs entirely on your computer. It has no servers and collects no
analytics or telemetry.

## Google Drive backup

If you sign in with Google (Settings → OmpDeck → Google Drive backup), OmpDeck requests:

- `drive.appdata`: read and write files in OmpDeck's own hidden app-data folder in your Google
  Drive. OmpDeck cannot see or change any other file in your Drive.
- `openid` and `email`: show which Google account is signed in.

OmpDeck uses this access only to upload, list, download and delete its own backups:

- **omp settings backup** (Settings → OmpDeck): configuration, custom models and agents, other omp
  config files, and prompt history.
- **Project sync** (Settings → Sync), only for the folders you add there and only the parts you switch
  on: a project's `.omp` folder (which can contain project settings and secrets you keep there) and
  that project's omp session transcripts, including their attachments.

Logins and API credentials of omp itself are never backed up. Backups go straight from your computer
to your Google Drive; they are not sent anywhere else, and nobody other than you can access them.

The sign-in token is stored only on your computer, in `~/.config/omp-deck/google-drive.json`
(readable only by your user). "Sign out" revokes it at Google and deletes the file. You can also
remove OmpDeck's access at any time at <https://myaccount.google.com/permissions>; to delete the
backups themselves, open Google Drive → Settings → Manage apps → OmpDeck → Delete hidden app data.

OmpDeck's use of information received from Google APIs adheres to the
[Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy),
including the Limited Use requirements.

## Contact

Questions: open an issue at <https://github.com/lavinhoque33/omp-deck/issues>.
