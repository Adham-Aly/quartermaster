Stop a background job and every process it started: its input is closed, then it gets SIGTERM, then SIGKILL. You're told how it ended.
- job: the job ID, e.g. J3.
