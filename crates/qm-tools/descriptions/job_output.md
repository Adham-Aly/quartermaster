Read a background job's output: everything new since your last job_output call for that job (all of it the first time), then whether it is still running or how it exited.
- job: the job ID, e.g. J3.
- filter: a regular expression; only matching lines are returned (optional).
