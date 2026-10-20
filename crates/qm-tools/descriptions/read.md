Read a file. Lines come back numbered from 1, like `cat -n`. PNG, JPEG, GIF and WebP images are shown to you. PDFs and other binary files are refused: use bash with a converter instead.
- file_path: the path; a relative path is taken from your worktree.
- offset: the first line to read, from 1 (optional).
- limit: how many lines to read (optional; at most 2,000 by default). Read long files in parts.
Lines longer than 2,000 characters are cut. Reading a file is what lets you write over it or edit it later.
