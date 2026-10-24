Replace exact text in a file. Read the file first in this session; if it changed since, read it again. old_string must match the file exactly, including indentation, and occur exactly once, unless replace_all is true: if it isn't found, or occurs more than once, nothing changes and you're told why. Add surrounding lines to old_string to make it unique. Returns the changed lines, numbered.
- file_path: the path; a relative path is taken from your worktree.
- old_string: the exact text to replace.
- new_string: its replacement; it must differ from old_string.
- replace_all: replace every occurrence (optional).
Edits go anywhere the user's account reaches except the app's own state; any task's worktree stays writable.
