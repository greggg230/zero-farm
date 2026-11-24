# Changelog

Versions of the Zero Farm app (`farm-agent.exe` and the `ZeroFarm.exe` launcher). Installed copies
update themselves; see [README.md](README.md) for how updates, channels and rollback work. Each
version is a GitHub Release tagged `agent-v<version>` (from 0.3.0 the Mac app's: `agent-macos-v<version>`).

## Unreleased (0.3.22)

Fixes for the farm owner's own computer playing practice games with the PyTorch program. Friends' computers are not
affected, except for the first and third points, which help every computer a little.

- **A job no longer loses its place on the server while Zero Farm is running.** When the server handed out new games
  only a few at a time, Zero Farm asked for more every ten seconds - so often that the server started refusing its
  once-a-minute check-ins too, and after three refused check-ins the server took the job away and all of its unfinished
  games were dropped. Zero Farm now asks a little less often when that happens, always keeps room for its check-ins,
  and repeats a refused check-in within seconds instead of a minute later.
- **A new version of a run's job program starts with the right number of games.** Each release of a game gives its job
  program a new name, and Zero Farm used to forget how much memory a game needs each time: it started with about 200
  games at once on a computer that then worked its way up to 600-1,500. It now starts a new version from what the
  previous version of the same run needed, and corrects it as before if the new version differs.
- **"Graphics card out of memory" with most of the card free is now understood.** A job program is given a memory limit,
  and on Windows the memory it uses on the graphics card counts toward that limit. When a program reached its limit, the
  card refused it more memory although the card was mostly empty; Zero Farm blamed the card and planned less of the
  card's memory for the run. It now sees that the program's own memory limit was the cause and restarts it with fewer
  games. And a program that was already finishing its games near its limit is stopped just before the limit (its last
  games go back to the server) instead of crashing into it.
- **The memory plan for the PyTorch program is measured, not guessed.** That program needs several GB before it plays
  a single game. Zero Farm assumed 1.8 GB and counted the rest as memory "per game", so the fewer games a job had, the
  more each game seemed to need - and the number of games shrank through the day, with a restart each time. Zero Farm
  now measures what the program needs before its games and counts the games' memory over that.
- **On the owner's computer, `max_ram_mb` means what it says.** The memory limit of a job used to be that number or
  the memory free at the second the job began, whichever was lower - on a computer whose training program comes and
  goes, anything from 11 to 26 GB. With `owner on` and `max_ram_mb` set, the limit is now always that number. (Work
  still stops when the computer runs low on memory.)

## Unreleased (0.3.21)

- **Programs with empty files can be installed.** Zero Farm could not download a file of zero bytes: it skipped the
  download (nothing to fetch) and then failed with "setup failed: network: The system cannot find the file specified".
  The PyTorch program has 93 such files, so no computer could install it. An empty file is now created, checked and
  put in place like every other file.
- **A big computer keeps its games going after it had to drop some.** When Zero Farm replaces a job program with one
  that needs the old one's memory, the old program's unfinished games are dropped. The server went on counting those
  as "being played" until the whole job ended - against the computer's own limit of games (so it got new games only
  once a minute, in lumps, and played with 10-25 % of its seats empty) and against the run's limit for all computers.
  Zero Farm now tells the server how many games it dropped. (Takes effect once the server is updated.)

## Unreleased (0.3.20)

- **New networks reach a running job within seconds, also while it winds down.** A computer used to hear about a new
  network only with its once-a-minute check-in, and a job that was winding down (finishing its last games before
  ending) kept playing the old network until it ended - on one computer 8-9 minutes, every ten minutes. Zero Farm now
  hears about new networks with every request for more games too, asks every few seconds where the farm owner turned
  that on for the computer, and switches its last games to a new network while a job winds down. Practice games then
  reach the training with fresher positions, and fewer are thrown away as too old.
- **The farm owner can raise this computer's disk space limit from the server - only with your consent.** Zero Farm
  keeps its files under a disk space limit you can set (3 GB unless you changed it). Some of the farm's programs are now
  bigger than that (the PyTorch program's files are over 4 GB). When you agree, the farm owner can set a different limit
  for your computer; Zero Farm then uses the owner's limit instead of yours, and the tray menu says so ("set by the farm
  owner"). Your own setting is kept, and applies again as soon as the owner clears theirs.

## Unreleased (0.3.19)

- **An "owner machine" setting, for the farm owner's own computer only.** The owner's desktop trains its networks and
  can now also play all of their practice games through Zero Farm, on the same graphics card. `owner on` makes Zero Farm
  work there whenever it isn't paused - also while the owner uses the computer or a fullscreen app runs -, share the
  graphics card with the training program instead of stepping aside for it (the temperature limit stays), run its job
  programs at normal priority, run one job program on the graphics card (several programs on one card take turns
  rather than work at once), and keep 12 GB of the card's memory free for the training program (`gpu_reserve_mb` changes
  that). A fixed layout can be set too: how many job programs, how many games each, in one or two groups, and how many
  search threads each. On Windows these are set with `farm-agent set` while Zero Farm is not running. Nothing changes on
  friends' computers: the setting is off unless set, and their own settings keep their limits (except that a fixed
  number of job programs may now be up to 8).
- **PyTorch practice games on computers the farm owner picks.** A computer the owner opts in on the server can play its
  practice games with the same PyTorch program the training uses, which is much faster on big NVIDIA cards than Zero
  Farm's own program. Zero Farm downloads that program's files (a Python with PyTorch) only for such a computer, checks
  once that PyTorch can use the graphics card, and then runs one such program for the job. Friends' computers never get
  it unless the owner opts one in; nothing else changes for them.

## Unreleased (0.3.18)

- **The limit on games at once now works on short jobs too.** 0.3.17 needed a job to run long and steadily before it
  could work out how many games finish in time, and on runs whose jobs last about ten minutes it never got there. Zero
  Farm now learns how much searching a game of the run takes from the first games of each job - also while a job winds
  down - remembers it for that run, and starts the next job with the right number of games at once instead of starting
  full and cutting back (cutting back throws away unfinished games).

## Unreleased (0.3.17)

- **Fewer games at once where too many would arrive too late to count.** The training program only keeps a game's
  positions while the network that played them is a few versions old, so a game that takes longer than about 5-10
  minutes loses its first positions. Zero Farm used to raise the number of games a computer plays at once as far as it
  made the computer faster, which on some computers made every game take 10-25 minutes, and up to 72 % of their
  positions were thrown away. When the server says how long a game may take for a run, Zero Farm now measures how many
  games the computer finishes per second and plays no more games at once than finish within that time (never fewer than
  enough to keep the graphics card or processor busy). A computer whose games already finish in time is unchanged.

## Unreleased (0.3.16)

- **More games at once on NVIDIA graphics cards, and no more costly layout trials there.** On a computer with an NVIDIA
  card, Zero Farm now starts the card's job programs the way it already starts them on DirectML cards: up to three
  programs of about 1,150 games each, fewer games each where memory is short, and only then fewer programs - never a
  single program when two fit. It also stops trying other layouts during a job on these cards: each trial had to stop
  the running programs' unfinished games (more than a thousand at a time) to free their memory for the next ones. On a
  4-core PC with an RTX 4070 Ti this played 38 % more games per minute in a test (336 against 244).

## Unreleased (0.3.15)

- **A job program that stops playing is started again.** After a pause, a job program has to set its graphics card up
  again; when it couldn't - another program still held the card's memory - it said so, played nothing more, but kept
  answering Zero Farm, which took it for busy: on a PC in November its job sat like that for an hour and a half until
  the farm ended it. Zero Farm now watches that a job program's games move on. One whose set-up failed after a pause
  is started again within a minute (on a graphics card with smaller batches, which fit), and one that plays nothing
  for five minutes while it has games running and gets the processor is started again too. Slow graphics cards and
  busy processors aren't affected: even the slowest job programs on the farm move on every few seconds.

## Unreleased (0.3.14)

- **A new training run starts with what the computer already knows about its program.** Zero Farm measures how much
  memory each game of a job program takes on the computer, and plans how many games to run at once from that. It kept
  the measurement per training run, so every new run - and the owner starts new ones often, to try ideas out - began
  at a cautious guess of 45 MB a game until it had measured again, although the job program was the very same one: on
  a 16 GB PC that meant a few hundred games at once where the program needs about 3 MB a game, for the first hour or
  more of each new run. A run the computer hasn't measured now starts from what it measured for the same job program
  in another run (the largest such figure, to be safe), and its own measurements take over from there.
- **The best setup found is kept for a copy of a run.** Zero Farm remembers the fastest setup it found for each job
  program and network. A copy of a run made with a newer version of the owner's tools counted as a different network,
  because the run's description lists every setting those tools know - including ones that have nothing to do with how
  games are played - and its setup was searched for again from the start. That list no longer counts; setups found by
  earlier versions are kept.

## Unreleased (0.3.13)

- **Graphics cards with slow games no longer stall on game numbers.** The farm hands each computer only as many game
  numbers as it has games running, plus a few. Whenever Zero Farm switched setups while timing them, it cut the old job
  program's last games short, and the farm went on counting those games as still running until the whole job ended. On
  a DirectML card, whose games take many minutes, a few switches were enough for the farm to stop handing out new game
  numbers: each new job program started with nothing to play, its timing run could never start and was thrown away, and
  the card made about 8 games an hour. Zero Farm now tells the farm straight away which games it has given up, hands
  the unplayed games of a stopped job program to the next one, and tells the farm promptly when job programs start and
  stop. In a test that copies that card, the job went from about a tenth of its normal speed to about its normal speed.
- **Timing runs wait for game numbers instead of giving up.** When the farm holds back game numbers (it limits how many
  each computer has, and each round of training has a limit too), Zero Farm no longer starts timing a new setup, and a
  timing run already under way waits for the numbers rather than being counted as a failure and switching back. The
  card keeps playing the games it has, and the setup is timed once the numbers come again.

## Unreleased (0.3.12)

- **Graphics cards running DirectML start from a setup that suits it.** DirectML, which AMD cards (and some NVIDIA
  cards, when a run asks for it) use, spends a long, fixed time on every batch of positions however big, so it does best
  with big batches and three job programs sharing the card. Zero Farm 0.3.11 started such a card like any card that isn't
  NVIDIA's: one job program with a few hundred games at once, which on an RTX 4070 Ti made only about a quarter of what
  DirectML can do there. It now starts up to three job programs of about 1,150 games each - fewer games each, then fewer
  programs, where the computer's memory or the card's memory holds less - and still tries other setups afterwards.
- **A DirectML card is judged at the batch size it will really use.** Zero Farm compared DirectML with the processor at
  the small batches a cautious first memory estimate allowed, where DirectML is slow, so on a computer with little memory
  set aside for the farm it could leave the card unused. It now compares them at DirectML's own batch size.

## Unreleased (0.3.11)

- **AMD graphics cards can help again, through DirectML only.** Since 0.2.11 Zero Farm left AMD cards alone, because
  AMD's own GPU programs misbehaved on a PC in November. From 0.3.11 an AMD card runs jobs with Microsoft's DirectML -
  never AMD's own GPU programs - from a small runtime pack made for it, and only after its self-test passes on the PC;
  a run uses it only when it publishes a graph DirectML can play. The project owner can turn GPU work on a brand of
  graphics card off again at any time without an update; the PC then runs its jobs on the processor.

## Unreleased (0.3.10)

- **Timing setups on a graphics card no longer trips over the card's own driver.** The graphics driver's own processor
  time, which a job on the card causes, counted as "other programs" - 6 to 11 % on an idle RTX 4070 Ti - so timing runs
  were thrown away or never started. Zero Farm now leaves that driver time out.
- A memory figure rounded just below what a job program really used could be raised again at once; it no longer is.

## Unreleased (0.3.9)

- **Graphics cards hold more games at once.** Zero Farm planned about 4 MB of memory for each game on a graphics card
  where the job programs use about 2.6 MB, partly because memory the graphics driver loads in the first minutes was
  counted per game. It now allows for that start-up memory separately and rounds less, so the same memory holds more
  games - on an RTX 4070 Ti with 32 GB, two job programs of about 1,300 games instead of about 1,100.
- **Finding the fastest setup is less often interrupted.** Zero Farm's own work (packing and uploading results) no
  longer counts as "other programs" when it checks that the computer is quiet enough to time a setup.

## Unreleased (0.3.8)

- **Graphics-card setups are worked out afresh.** Zero Farm 0.3.5 and 0.3.6 counted every setup they had to put off as
  a failed try, and after three it never tried that setup again - on an RTX 4070 Ti that ruled out sharing the card
  between job programs for good. Zero Farm 0.3.8 starts the graphics-card search over, with 0.3.7's ability to try
  every setup.

## Unreleased (0.3.7)

- **Graphics cards find their fastest setup.** When the job programs sharing a graphics card filled the memory Zero Farm
  may use, version 0.3.6 could never try another setup (or even time the one it was running), so it kept its first
  guess. Zero Farm now winds the running setup down (at most two minutes, the unfinished games going back to the farm)
  and times each candidate, once it has measured how much memory the job really needs.
- **More work from the same graphics card.** Each job program now plays its games in two halves, so its share of the
  processor keeps working while the other half waits for the card: in a test on an RTX 4070 Ti, three job programs went
  from about 22,000 to as many as 28,000 positions per second, depending on how many games fit in memory.
- **No more "fullscreen app" on an idle computer.** Windows sometimes says a fullscreen app is running when nothing is on
  the screen at all; Zero Farm now checks that a window really covers the screen before it holds back, and notes in its
  log each time it pauses or resumes work and why.
- Job programs stopped together with a cancelled job are no longer logged as failures.

## Unreleased (0.3.6)

- **Graphics cards get their crew of job programs.** Zero Farm 0.3.5 counted 1.8 GB for each job program's own start-up
  memory and, on a computer whose other programs held half of its memory, gave the job less than a third of the RAM,
  so a 4070 Ti kept a single job program. Zero Farm now measures how much a job program really needs before its games
  grow, and while you're away it leaves an eighth of your memory free instead of a quarter (a quarter again as soon as
  you're back, and the job steps aside if memory gets tight): two or three job programs then share the card.
- **Switching setups no longer waits for the last slow games.** When a job program winding down held memory the next one
  needed, its last few games could take five minutes; they now end after two minutes and go back to the farm, which
  plays them again.

## Unreleased (0.3.5)

- **Graphics cards can get a network made for them.** A farm run can now publish, for each network, a second copy built
  for graphics cards (for example a fixed-size version that NVIDIA cards run faster). Zero Farm 0.3.5 downloads that copy
  when the farm sends your computer the graphics-card version of a job, checks it against the run's signed description
  exactly like the normal network (and refuses anything else), keeps it next to the normal one, and hands it to the job
  program. Computers without a suitable graphics card, and Macs, keep the normal network. Runs that use this need Zero
  Farm 0.3.5: older versions are asked to update first.
- **A graphics card reaches full speed within minutes.** For a job version it hadn't seen before, Zero Farm 0.3.4
  planned for far more memory per game than the job program uses, so an RTX 4070 Ti started with only 139 games at once
  and stayed there. Zero Farm now checks the real memory use within the first minutes and moves to two job programs of
  about 500 games each (on that card) as soon as they fit.
- **No more endless switching while finding the fastest setup.** When a graphics-card setup to try didn't fit in memory
  beside the running job program, Zero Farm 0.3.4 let the running one wind down alone for minutes, then often started
  the old setup again and tried the same switch a few seconds later. Now such a setup is put off (and given up after a
  few tries), and a try that can't be measured counts as tried.

## Unreleased (0.3.4)

- **Graphics cards are used properly for long jobs.** On a computer that plays the farm's long self-play jobs on its
  graphics card, Zero Farm 0.3.3 kept far too few games going at once (128 on an RTX 4070 Ti, with the card only a third
  busy), because it planned for much more memory per game than the job program really needs. Zero Farm now aims for two
  full batches of games for the card (about 1,000 on a 4070 Ti), learns the memory each game really takes during the job
  and plays more games at once as soon as it fits, without waiting for the next job.
- **Several job programs share the graphics card.** One job program can only hand the card one batch at a time, so on
  an NVIDIA card Zero Farm now starts two or three job programs side by side where the card's memory and the computer's
  memory allow (three of about 380 games each on a 4070 Ti: almost three times what 0.3.3 played), splitting the
  processor threads between them. They all step aside together - and give the card's memory back - when you use the
  graphics card for something else, and if one of them crashes the others carry on.
- **The card's memory is planned and watched.** Zero Farm plans how much of the graphics card's memory each job program
  may use, measures what it really uses, and keeps everything within what the card has: when another program fills the
  card's memory, one job program steps aside, and one that runs out of card memory starts again with smaller batches
  instead of failing.
- **The best setup is measured on your computer.** Finding the fastest way to use the processor now also covers the
  graphics card: how many job programs, how many games each and in how many batches, how many processor threads - tried
  only while you're away from the computer, it's plugged in and nothing else keeps it busy, kept only when clearly
  faster, remembered per card and driver, and shared with the farm.

## Unreleased (0.3.3)

- **Finding the fastest layout no longer goes round in circles.** When a long job's memory plan held fewer games than a
  layout remembered from an earlier job, Zero Farm 0.3.2 never counted the measurements of the smaller layout as the
  remembered one's and handed the job over to the same layout again and again, measuring nothing. A remembered layout
  is now compared as it actually runs, the job is never handed over to the layout it already runs except to measure
  it, and at most 6 handovers happen in an hour.
- **Two job programs stay within their memory while one takes over from the other.** Every handover left the old job
  programs finishing their games beside the new ones, and Zero Farm 0.3.2 judged the room for a new job program by what
  the others used right then - little, for one that had just started. On a 64 GB Mac four finishing job programs and
  the two new ones used 38.9 GB of a 31.7 GB share. Now a new job program starts only when its whole planned memory
  fits beside the others (one that just started counts at its plan), only one handover finishes its games at a time,
  and when the memory in use reaches 92 % of the share no job program starts new games until the others made room.
- **Memory-limited jobs keep the processor busy.** When memory limits how many games a Mac plays at once, the games
  stay in the same number of batches and each batch gets smaller (down to 96 games), instead of fewer, bigger batches
  that leave the search threads waiting: on a 64 GB Mac two job programs play about 700 games each in 6 batches.
- **Memory planned from what each job program really uses.** Games on a Mac grew to about 16 MB each after half an
  hour with the job programs of before (Zero Farm 0.3.2 planned 14), so a long job filled its share and had to slow
  down, while the fixed job program needs only about 2.5 MB a game. Zero Farm now learns the memory per game for each
  run and job-program version: a version it hasn't measured starts from a safe default (18 MB a game on a Mac), more
  use is remembered at once, less use - measured once the job has settled - lowers the plan, and when a job uses much
  less than planned it plays more games at once straight away.
- **Background programs no longer stop every measurement.** A Mac's background programs (sync tools, indexing) use the
  processor in short bursts, and Zero Farm 0.3.2 threw a measurement away at the first burst. It now looks at what other
  programs used over the whole measurement, compared with just before it: short bursts are fine, lasting use still
  stops it. Measurements still happen only while the computer isn't in use.

## Unreleased (0.3.2)

- **Macs no longer freeze at the start of a job run by two job programs.** On a computer that doesn't use its graphics
  card for the farm (every Mac), Zero Farm 0.3.1 mistook a job program that was still starting up for one that uses the
  graphics card, paused it straight away ("GPU is busy") and never let it go on, so the job sat there until the farm
  took it back after 10 minutes - every time two job programs shared a job. A job program now counts as a processor
  job until it says otherwise.
- **Macs take the farm's Mac version of a job.** When a run offers a version of its job made for Macs, Zero Farm 0.3.1
  on a Mac refused it as graphics-card work (and reported a problem); it now runs it on the processor, as intended.
- **Long jobs on a Mac plan their memory from real measurements.** A Mac's games use more memory as they go than Zero
  Farm 0.3.1 expected (about 13 MB each once a job has run a while, not 4), so a long job filled its share of memory
  and had to slow down part-way through. Zero Farm now plans for that from the start: on a 64 GB Mac a long job plays
  about 1,900 games at once in one job program, or about 920 in each of two.

## Unreleased (0.3.1)

- **Zero Farm finds the fastest way to use your computer's processor.** For long self-play jobs it now tries a few
  ways of splitting the work between processor cores (how many work on game search, how many on the network, how many
  games at once and in how large batches) and keeps the fastest - measured on your computer, while it does real work
  for the farm (nothing is thrown away). It only tries while you're away from the computer, it's plugged in and nothing
  else keeps it busy; if you come back mid-try, that try is dropped and it goes back to what worked best. The result is
  remembered (and checked again weekly) and shared with the farm, so computers like yours start from it. It never uses
  more processor threads than before. `ZeroFarm status` on a Mac and the tray menu on Windows show the current choice;
  `ZeroFarm set autotune off` (Mac) turns it off.
- **Big computers run several jobs side by side.** On a computer with many cores and plenty of memory, one job program
  can't keep every core busy; two of them side by side play about 30 % more on a 16-core Mac. Zero Farm now runs a long
  self-play job as up to three job programs at once where the processor and memory allow it, sharing the job's games
  and its memory between them - a Mac with at least 12 cores and 32 GB starts with two, other computers try it while
  tuning and keep it only if it's faster. They pause, resume and stop together, and if one of them crashes the others
  carry on. `ZeroFarm set workers 1` (Mac) keeps it to one; `workers auto` (the default) lets Zero Farm choose.

## Unreleased (0.3.0)

- **Zero Farm for Macs with Apple silicon.** The same app now runs on a Mac (macOS 13 or newer): install it with
  `curl -fsSL https://farm.greggg230.com/install-macos.sh | sh -s -- --invite CODE` (without `--invite` it asks for the
  code). It lives in `~/Library/Application Support/ZeroFarm`, starts at login and updates itself like the Windows app.
  There is no menu-bar icon yet: `ZeroFarm status`, `pause [minutes]`, `resume`, `set <setting> <value>`, `quit` and
  `uninstall` work from the Terminal (the setup prints the full path). It works while the Mac is idle and on power,
  uses every processor core at the lowest priority (it gives way to anything you do), and keeps the Mac from going to
  sleep while it works on power (the display still sleeps; `set keep_awake false` turns that off). Macs run processor
  jobs only for now.
- **For a Mac of your own: work while it's in use, and on battery.** `set work_when active` (the Windows tray's
  "always at low priority") and the new `set on_battery true` (also in Low Power Mode) let it work regardless;
  the defaults stay as they were.
- Zero Farm tells the farm which kind of computer it runs on, and refuses (and reports) a job program built for
  another kind before downloading it.

## 0.2.14 (2026-11-17, canary)

- **Long self-play jobs no longer run out of memory.** Zero Farm now plans how many games a long job plays at once
  so that they fit comfortably within the memory it may use, and keeps an eye on the job's memory while it runs.
  When the job gets close to its limit, Zero Farm lets the games in progress finish and carries on with fewer games
  at once, so nothing is lost and the job keeps going. If a job runs out of memory anyway, Zero Farm starts it again
  with fewer games instead of giving the work back to the farm (only the games that were in progress are lost).
  Either way it remembers how much memory that run's games need on this PC and plans its next jobs with it.

- **The farm can now tell a PC that's in use from one that's switched off.** While Zero Farm is waiting
  because you're using the PC (or a fullscreen game is running, it's on battery, it's outside the hours
  you picked, or you paused it), it now tells the farm why, every 5 minutes. Before, it said nothing at
  all while waiting, so the farm showed the PC as offline. Nothing about when Zero Farm works changes,
  and the message is only the reason (for example "user_active"): nothing about what you're doing.

## 0.2.13 (2026-11-17, canary)

- **Long self-play jobs that keep the PC's processor or graphics card fully busy.** For runs that use them, a job
  now keeps a steady number of games going: whenever one ends, the next starts, and Zero Farm asks the farm for more
  games as it goes, so the work never trails off at the end of a job. Finished games are sent every couple of
  minutes. When the farm's network improves, the job switches to the new network in place, after checking that the
  farm's signed instructions for it are genuine and unchanged otherwise. Such a job ends by itself when the farm has
  other work for the PC, when its instructions change, or when a Zero Farm update is ready, so updates still happen.
- How many games run at once: 32 on the processor; on a graphics card, as many as still make it noticeably faster
  (measured on the card), never more than fit the memory Zero Farm may use.
- **One job whose results the farm keeps refusing no longer holds up the others.** Before, Zero Farm sent results
  one job at a time and stopped at the first that failed, so a single stuck job could keep every later job's
  results from reaching the farm until they were too old to use. Each job now retries on its own schedule.

## 0.2.12 (2026-11-15, canary)

- **"Automatic" CPU threads now uses one thread per physical core** (it was physical cores minus 2), still at
  least 1 and never more than the processor's logical threads. Jobs run at the lowest priority, so they still
  give way to anything you do. Only PCs on "Automatic" change; a number picked in the tray's "CPU threads" menu
  stays as it is. The tray's "Automatic (N)" label now always shows the automatic number, even while another
  number is picked.

## 0.2.11 (2026-11-13, canary)

- **A GPU check that takes the PC down is never repeated over and over.** Before testing a graphics card, Zero
  Farm notes on disk that a test is running. If the PC or the app stops during the test (a graphics driver
  crash, a frozen PC that had to be restarted), the next start sees the note, tells the farm, and doesn't test
  that runtime pack on this card again for a day; after a second time, never again with this driver.
- **AMD graphics cards are not used for farm work.** PCs with only an AMD card run jobs on the processor, as
  before the GPU support; Zero Farm doesn't test the card at all.
- Zero Farm removes builds the farm has withdrawn from its download folder when it starts.
- **Logging off or shutting down Windows no longer counts as a crash.** The launcher could take the app being
  stopped by Windows at logoff for three crashes and go back to an older version; now it waits for the
  session to end instead of starting the app again.

## 0.2.10 (2026-11-13, canary)

- **NVIDIA graphics cards run networks with CUDA**, 5 to 14 times faster than DirectML on the same card. The
  CUDA runtime pack is large (about 1.5 GB on disk, 0.7 GB to download, compressed), so it is fetched only on a
  PC with an NVIDIA card and a recent enough driver, while "Use the GPU" is on and the disk limit has room, and
  only for jobs that use the GPU. Before downloading it, Zero Farm checks all of this itself: a PC without an
  NVIDIA card never downloads it. It is downloaded once and kept; if its GPU test fails on this PC, it is
  removed and not downloaded again for that driver.
- **The GPU is used only when it is faster.** Before a job's first game, Zero Farm measures the job's own
  network on the processor and on each graphics provider that passed its test, at the job's batch sizes, and
  runs the GPU only if it is clearly faster than the processor (small jobs often run faster on the processor).
  GPU jobs then play many games at once, so the card gets big batches.
- Build files can be downloaded compressed (zstd), checked before and after unpacking.

## 0.2.9 (2026-11-12, canary)

- Release-path check: no user-visible changes. 0.2.8 must update itself to it before 0.2.8 goes
  to the stable channel.

## 0.2.8 (2026-11-12, canary)

- **Jobs that name split and double-faced cards can run.** A job's signed instructions may now name
  cards such as "Rough // Tumble" or "Tamiyo, Inquisitive Student // Tamiyo, Seasoned Scholar".
  Earlier versions refused them, taking the " // " in the card's name for part of a file path.
  Anything else that looks like a path is still refused.
- **The "Join Zero Farm" window names the third-party license terms.** One line says that by
  installing you accept the license terms of the third-party components Zero Farm includes
  (Microsoft's ONNX Runtime, Windows ML runtime and DirectML, and the Visual C++ runtime), with a link
  to their list in the [README](README.md#third-party-components).

## 0.2.7 (2026-11-12, canary)

- **Graphics cards from every vendor** (NVIDIA, AMD, Intel) can now be used for network jobs. Zero Farm
  only uses a GPU after testing it on this PC: the network's answers on the GPU must match the processor's
  exactly (within a tiny tolerance), or the GPU isn't used.
- **It stays out of your way on the GPU too:** it notices when any other program uses the graphics card
  (for every vendor, the way Task Manager does) and waits, and it watches the card's temperature where
  Windows reports it.
- PCs with one graphics card listed twice by Windows now report it once.

## 0.2.6 (2026-11-12, canary)

- Release-path check: no user-visible changes. 0.2.5 must update itself to it before 0.2.5 goes
  to the stable channel.

## 0.2.5 (2026-11-12, canary)

A security update, from an independent review of the code.

- **Job instructions come only from signed definitions.** What a job may make your PC download and
  run (which job programs, which network files with their sizes and SHA-256, the exact job
  settings) is now fixed by a definition Gregg signs with a separate *data key*. Zero Farm checks
  its signature, that it hasn't expired and isn't older than one it has seen, that every network
  file has the size and hash it names, and builds the job program's instructions only from it plus
  the few numbers the farm fills in per job (which games or seeds) and its own file locations.
  Before, the job program got most of a job's instructions as the farm server sent them, so the
  server, not Gregg's signature, decided them. A job that fails a check is refused, reported once a
  day, and Zero Farm waits before
  asking again. Jobs from today's farm server (before it sends definitions) go through a strict
  allowlist instead: only the fields a job needs, and nothing that looks like a file path.
- **No more "This PC was removed from the farm" by mistake.** Only the farm server's own "revoked"
  answer makes Zero Farm forget its sign-up. An error page from something in between (a firewall, a
  proxy, Cloudflare's protection) is now treated like a network problem and retried. If the farm
  refuses this PC's sign-in, the tray says "This PC can't sign in to the farm; ask Gregg." and Zero
  Farm keeps trying, instead of throwing its sign-in away.
- **Logs are redacted.** Your Windows user name and your user folder (`C:\Users\<you>`) no longer
  appear in Zero Farm's logs, and are removed from everything it uploads: the logs (also older
  ones), crash reports and performance numbers.
- The NVIDIA GPU library (`nvml.dll`) is loaded only from `C:\Windows\System32`, never from another
  folder where another program could have put a file of that name.
- A network file is never downloaded unless the space it needs fits under the 3 GB cap first.

## 0.2.4 (2026-11-12, canary)

- Release-path check: no user-visible changes. 0.2.3 must update itself to it before 0.2.3 goes
  to the stable channel.

## 0.2.3 (2026-11-12, canary)

- **Evaluation jobs**: two or more trained networks play each other, for example to see whether a
  new network beats an older one. Zero Farm downloads each network file the job names (about
  13 MB each, kept in its cache), checks its SHA-256, lets the job program self-test every network
  before the first game, and sends back one small row per game.
- **When Windows blocks the job program** (Smart App Control, a company's application control, or
  an antivirus): the tray now says "Windows is blocking Zero Farm's job program (Smart App
  Control). Zero Farm can't work on this PC until its programs are code-signed. Nothing is wrong
  with your PC." Zero Farm tells the farm once a day and then waits before trying again (15 min,
  1 h, 4 h, 16 h, then once a day). Before, it asked for new work again at once, hundreds of times
  an hour.
- Any other job program that fails to start is reported and retried later (1 minute, doubling up
  to an hour), never in a tight loop; so is a job whose files can't be downloaded or don't match
  their checksums.
- Fix: clicking the tray icon could close Zero Farm ("RefCell already borrowed", seen three times
  in 10 s on one PC). The tray no longer re-enters itself while a menu or message box is open, and
  a problem in the tray is now reported to the farm instead of closing the app.

## 0.2.2 (2026-11-12, canary)

- Release-path check: no user-visible changes. The first version built from this public
  repository; 0.2.1 must update itself to it before 0.2.1 goes to the stable channel.

## 0.2.1 (2026-11-12, stable)

- The first stable release. No user-visible changes from 0.2.0; it proved on a canary PC that
  0.2.0 updates itself, launcher included, and then updated itself to 0.2.2 the same way.

## 0.2.0 (2026-11-12, canary)

- The launcher (`ZeroFarm.exe`) can now update itself too: a new launcher must pass a self-test
  and then take over; if it doesn't, the old one puts itself back and carries on.
- **Send diagnostics** in the tray menu uploads Zero Farm's logs when you want to report a problem
  (at most once an hour).
- A PC that is busy with other programs no longer holds on to jobs it can't get CPU time for:
  being starved of CPU counts like a pause, and the job is handed back after about 10 minutes.
- The farm now sees how available each PC is (idle hours and running hours per day, a 7-day
  average), to hand out the right amount of work.
- Jobs are not allowed to touch the clipboard, desktops, display or system settings, or log you
  off.
- An installer too old to join tells you where to get a new one, and your invite code stays
  valid.
- The round green "Z" icon and version information on every program (Explorer → Properties).

## 0.1.3 (2026-11-11, canary)

- Fix: when the farm required an update that couldn't be installed (the required version had been
  rolled back on that PC), the app asked for work several times a second. It now waits 5 minutes
  between attempts.

## 0.1.2 (2026-11-11, canary only, withdrawn)

- A deliberately broken build that crashes at start, published only to the canary channel to test
  automatic rollback on a real PC. The launcher went back to 0.1.1 after 3 crashes and marked
  0.1.2 as bad. Never offered to the stable channel; 0.1.3 replaced it.

## 0.1.1 (2026-11-11, canary)

- Silent self-update test from 0.1.0; no user-visible changes.

## 0.1.0 (2026-11-11, canary)

- First release: per-user installer (no admin rights), sign-up with an invite code, tray icon and
  menu, the idle rules (5 minutes without input; never in fullscreen apps, on battery or with low
  memory; stops within 2 seconds), signed and verified downloads, jobs at the lowest priority in a
  memory-capped job object, results that survive restarts, silent self-update with automatic
  rollback, uninstall from Apps & features.
