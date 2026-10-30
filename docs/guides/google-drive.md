# Google Drive setup

Ingot uses your own Google OAuth client. None ships with the repo, because the repo is public.

1. In the [Google Cloud console](https://console.cloud.google.com/), create a project and enable the **Google Drive API**.
2. Configure the OAuth consent screen and add your account as a test user. While the app is in "Testing", Google expires refresh tokens after 7 days, so you will need to sign in again; publishing the app avoids that.
3. Create an OAuth client of type **Desktop app**.
4. Sign in:

   ```sh
   export INGOT_GDRIVE_CLIENT_ID=...
   export INGOT_GDRIVE_CLIENT_SECRET=...
   cargo run --release -- gdrive-login
   ```

   The refresh token is saved to `~/Library/Application Support/ingot/gdrive-token.json`, readable only by your user.

5. Import with a local destination and Drive together:

   ```sh
   cargo run --release -- import /Volumes/EOS_DIGITAL/DCIM --dest ~/Photos --subfolder Wedding --gdrive
   ```

The card is read at the speed of the local destination. Uploads carry on after the card is finished, using the local copy.

## What Ingot can touch

Only its own `__ingot_backups` folder in My Drive. The name is fixed and no option changes it. Your `--subfolder`, organize and rename choices apply inside it.

- Creating and listing take a folder handle that the code can only get from `__ingot_backups` or a folder already inside it. Paths that try to climb out are refused.
- Ingot has no code that deletes, moves, renames or changes anything on Drive.
- It asks Google for the `drive.file` scope, so its token can only see files Ingot created. It can't read the rest of your Drive, and it won't find photos you uploaded another way, so those aren't treated as duplicates. A folder you made yourself named `__ingot_backups` is invisible to it too, so Ingot would create its own beside it.

## Uploading an existing local folder

To fill in Drive for a folder that is already on disk, use that folder as both source and destination. Local files already there are recognised by hash and not rewritten:

```sh
cargo run --release -- import ~/Photos/Wedding --dest ~/Photos/Wedding --organize original --all --no-previews --gdrive
```

