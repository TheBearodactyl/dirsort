# Command-Line Help for `dirsort`

This document contains the help content for the `dirsort` command-line program.

**Table of Contents:**

<!--toc:start-->
- [Help for `dirsort`](#command-line-help-for-dirsort)\
      - [**Flags**](#dirsort)\
      - [**Default Configuration**](#default-configuration)
<!--toc:end-->

## `dirsort`

**Usage:** `dirsort [OPTIONS]`

###### **Options:**

- `-o`, `--output-dir <OUTPUT_DIR>` — The directory to sort the files into
- `-n`, `--notify` — Send a notification when finished
- `-m`, `--move` — Move files instead of copying them
- `-b`, `--blacklist <BLACKLIST>` — Extensions to exclude from sorting (comma-separated, e.g., 'txt,log,tmp')
- `--blacklist-file <BLACKLIST_FILE>` — Path to file containing blacklisted extensions (one per line)
- `-j`, `--threads <THREADS>` — Number of threads to use for parallel processing (default: number of CPU cores)
- `-d`, `--max-depth <MAX_DEPTH>` — Maximum depth to recurse into directories (0 = current directory only, default: unlimited)
- `-c`, `--config <CONFIG>` — Path to a configuration file with desired categories ([**default**](#default-configuration))
- `-i`, `--index` — Generate an HTML index file after sorting

<hr/>

## Default configuration

```toml
[categories]
Images = ["gif", "ico", "jpeg", "jpg", "jpg~", "png", "png~", "webp"]
Videos = ["mp4", "mkv", "ogv", "webm"]
Documents = ["pdf", "docx", "doc", "txt", "md"]
Audio = ["mp3", "wav", "flac", "ogg"]
Archives = ["zip", "tar", "gz", "rar", "bz"]
```

<hr/>

<small><i>
    This document was generated automatically by
    <a href="https://crates.io/crates/clap-markdown"><code>clap-markdown</code></a>.
</i></small>

