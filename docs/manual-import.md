# Importing an existing library

If you already have anime on disk, you do not have to download it again. **Library → Import** points Ryokan at a folder you already have, reads the filenames, matches each series on AniList, and shows you exactly what it would bring in before anything changes on disk.

!!! note "Preview only in this release"
    The import step itself (creating the series, hardlinking or copying the files under the media root, tagging each episode) is the second half of this feature and is not in this release yet. The preview stands on its own as a check of what Ryokan makes of your library: which folders it recognizes, which AniList entry each one maps to, and which files it cannot place.

## Before you start

- Set a media root under **Settings → General**. Imported files are placed under it, and the wizard will not start without one.
- Point the wizard at a folder **outside** the media root. Everything inside the media root is already Ryokan's, and the wizard refuses to scan it.
- In Docker, the folder has to be mounted into the container, and the path you type is the container path (for example `/library`, not `/srv/media/old-library`).

## Scanning

The start form takes:

- **Folder to scan**: an absolute path. Ryokan walks it recursively, up to eight levels deep.
- **How files reach the media root**: Hardlink (recommended), Copy, or Move. Hardlink keeps the original in place and uses no extra space when both folders are on the same filesystem. Copy doubles the disk use. Move frees the source folder. Hardlinks cannot cross filesystems, so if the source and the media root are on different ones the preview warns you that hardlink mode would copy.
- **Follow symlinks**: off by default. Libraries that symlink into a downloads folder would otherwise import the same file twice.
- **Include hidden files and folders**: off by default. NAS sidecar folders such as `@eaDir`, `.AppleDouble`, and `lost+found` are always skipped.

Only video files count (`.mkv`, `.mp4`, `.avi`, `.wmv`, `.webm`, `.m4v`, `.ts`). Ryokan's own media root and recycle bin are skipped even when they sit inside the folder you chose. Folders it cannot read are counted and skipped, not fatal.

Matching each series is one AniList lookup, and AniList allows about thirty a minute, so a library with hundreds of shows takes a few minutes on the first scan. The page updates itself when the preview is ready; you can leave it and come back through the Library page. A preview stays available for two hours after you last touched it.

## Reading the preview

The strip at the top counts series and files, how many are new, how many are already in your library, how many Ryokan could not match, and how many files it would write. Below it, one card per series, colored by outcome:

- **New series** (green): matched on AniList and not in your library. Importing would create it.
- **Already in library** (blue): matched to a series you already track, by AniList id (or MAL id for series added through the MAL fallback), never by comparing titles. The card links to the existing series page.
- **No match** (gray): AniList returned nothing for the name Ryokan read. Search again with a different title, or skip it.
- **Skipped**: you excluded the whole series.

### How Ryokan reads names

The series name comes from the filename first (`[SubsPlease] Sousou no Frieren - 05 (1080p).mkv` reads as *Sousou no Frieren*). When a filename carries no name at all (`01.mkv`, `S01E05.mkv`, `Episode 07.mkv`), Ryokan uses the folder above it, skipping folders like `Season 01`, `Specials`, or `Extras` on the way up, so `Anime/Naruto/Season 01/01.mkv` reads as *Naruto*. Rows that took their name from the folder carry a small **folder** tag.

Episode numbers use the same parser as the rest of Ryokan, so the `S01E07` in the preview is the number the import would record. Files that Ryokan cannot number (creditless openings and endings, bare specials, files with no digits) are listed but marked **No episode number** and would not be imported.

Files with a season number past the first (`S02E01`) form their own series and are searched as "title season 2", because AniList lists each season separately. A year in a filename or folder name (`Hunter x Hunter (2011)`) is used to prefer the right remake.

### The file table

Each row shows the file, its episode, the quality Ryokan reads from the filename, what the import would do with it, and where it would land. The result column is one of:

- **Import**: the episode is not in your library yet.
- **Replace**: you have the episode at a lower quality and the import would upgrade it.
- **Already have**: you have it at equal or better quality; the file is left alone.
- **Downloading**: a grab for this episode is in flight.
- **Pinned**: the existing episode has a manual quality override and is never touched.
- **No episode number**: see above.
- **Excluded**: you unticked it.

Files land at `<media root>/<series folder>/Season 01/<original filename>`. Filenames are kept as they are; renaming into Ryokan's own naming scheme is a separate feature. For a new series the folder name is generated from the AniList title, and if a folder of that name already exists under the media root without a series owning it, the preview shows the suffixed name (`Show (2)`) the import would use instead.

### Badges worth a look

- **Check this match**: the AniList title shares little with the name Ryokan read. Look at the alternatives before trusting it.
- **Also matched by ...**: two folders resolved to the same AniList series (a rename, a duplicate rip). Both would import into that one series.

## Correcting a match

Every card has the same controls, and each one updates just that card:

- **Pick another**: the next-best AniList results, one click each.
- **None of these**: drop the match. The card becomes a no-match card until you search again.
- **Search again**: type any title and Ryokan re-runs the AniList search for that series only. Your file ticks survive the re-search.
- **Skip**: exclude the whole series. **Include** brings it back.
- The checkbox on each row excludes or includes that file; **All** and **None** in the header do the whole table.

## Files with no series hint

Files where neither the filename nor any folder above them names a series are listed at the bottom under **Files with no series hint**. Rename the file, or move it into a folder named after the show, and scan again.

## What the preview does not do

- Nothing is written, moved, or tagged. The **Discard preview** button forgets the scan; so does leaving it untouched for two hours.
- The quality shown per file is read from the filename only. The import step runs the full classifier (ffprobe and all) the way post-processing does.
- Folders named after something AniList does not know (`misc`, `To sort`) become no-match cards rather than being hidden. Skip them or search for the right title.
