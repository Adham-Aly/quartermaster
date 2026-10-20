Declare what a failing check on your latest push is, before repairing it. Free to call. At the standard and reviewed levels pr_open refuses a fix for a failing check without one.
- check: the failing check's name.
- diagnosis: own_regression (your change broke it: your next push uses one of 3 repair attempts if the same check fails again; the third holds the task for the user), suspected_flake (lets you rerun it once per task with ci_rerun), or failing_on_main (then file the separate task with file_followup and wait on it with declare_wait; waiting costs nothing).
- evidence: what shows it; required for suspected_flake and failing_on_main (for failing_on_main, a reproduction on an untouched main).
