---
name: agentctl
description: How to act in the chat beyond your reply with the agentctl command - attach files to your reply, post elsewhere, react to messages, read more of the thread, lock shared/ for writes, hand a task to another agent, or ask for a task on your owner's private resources. Use it whenever the user asks for any of these.
---

# agentctl

`agentctl` is how you act in the chat beyond the text of your reply. Run it
with the Bash tool. It works only while a turn is running: between turns
every command is refused.

Each command prints one short line of plain text when it works. When it is
refused or fails, it prints `agentctl: <reason>` on standard error and exits
with status 1; the reason says what to change. A usage mistake exits with
status 2 and prints the usage.

`react` and `history --before` name a message by the id the conversation
shows you, such as `#7`, or by its platform id (a Slack timestamp, a
Rocket.Chat id). `post --to` takes platform ids only.

## attach

```sh
agentctl attach <path>
```

Stages a file to upload with this turn's reply. Files are uploaded before
the reply's text. Write the file first, anywhere you can write, then attach
it.

Refused when the file is over the size limit (50 MB by default), when a turn
already staged 10 files, or when the file name holds `/`, `\`, control or
invisible characters, or is `.` or `..`.

## post

```sh
agentctl post --to <target> <text>
```

Posts Markdown text somewhere else. It is queued and sent after this turn,
after your reply. `<target>` is:

- `here`: this thread (in a direct message, the conversation);
- a conversation id, such as `C123` or `#C123`: that conversation's top
  level;
- `<conversation id>/<message id>`: the thread under that message.

Refused on the public side (every channel turn, and anything a member other
than your owner asked for) unless the target is this conversation; use
`--to here`. In your owner's direct messages you may post in any
conversation your bot is a member of; the chat platform refuses the rest when
the post is delivered. Also refused when the text is empty or over 40,000
bytes, or when the turn already queued 10 posts. Channel names such as
`#general` are not looked up; use the conversation's id.

## react

```sh
agentctl react <emoji> [message id]
```

Adds a reaction, such as `eyes` or `white_check_mark`, after this turn.
Without a message id it reacts to the message that started the turn.

Refused for a message outside this conversation, on either side, and when
the turn already queued 20 reactions. You can also react by writing
`[[react: eyes]]` anywhere in your reply; it is removed from the text.

## history

```sh
agentctl history [--before <message id>] [--limit <n>]
```

Prints more of this thread than the turn showed you, oldest first: at most
`--limit` messages (1 to 200, default 50), older than `--before` if given.
Each message has a header line with its id, sender and time.

Refused when `--before` names a message outside this conversation, or one
this session doesn't know. Not available on every platform.

## lock

```sh
agentctl lock [--timeout <seconds>] -- <command> [args...]
```

Runs a command while holding this scope's lock on `shared/`, the directory
every conversation of this scope shares. Take it for every write to
`shared/`, so two conversations don't write at once. Another `lock`, from
this conversation or another, waits until the first command ends.

The command runs directly, not through a shell: use `sh -c '...'` for pipes
or several commands. `lock` exits with the command's status.

It gives up after `--timeout` seconds of waiting (100 by default). If
agentd stops renewing the lock (the turn ended, or agentd refused), the
command is killed and `lock` exits 1 with "lost the shared/ lock". Refused
inside a private task.

## ask-agent

```sh
agentctl ask-agent <agent> <task>
```

Hands a task to another agent through agentd's policy. The other agent
answers in this thread, and its turn is billed to this turn's requester.
Refused inside a private task, and when policy doesn't let this turn's
requester use that agent. Not available yet on this server: it answers
"not available yet".

## private

```sh
agentctl private [--file <path>]... <task>
```

Asks for a task on your owner's private resources (their `shared/`
directory and private tools), which this conversation can't reach. It
returns a consent id at once and does not wait for the task. Unless this
turn is your owner's own direct message with you, your owner is asked to
approve the task first, and sees it exactly as you wrote it, so write it
plainly: invisible or control characters other than newlines and tabs,
indentation past 32 columns, blank runs wider than 16 columns (a tab is 8) inside a
line, more than 2 blank lines in a row and heavily stacked accents are
refused; the task text may be at most 3000 characters, an
emoji counting as two. When the task
finishes, agentd posts its result to this thread; you won't see it in this
turn, so tell the requester that the result will follow.

`--file` hands a file from this session's directory (your working
directory, or anything else under the session directory) to the task, as it
is now; repeat it for several, up to 10, each named differently and none a
dotfile or `CLAUDE.md`, together no larger than one attachment may be. The
task finds them in its working directory. Nothing else of this conversation
reaches the task: put what it needs in the task text or a file. Its result
comes back as a new message in this thread, headed `Private task <id>:`,
with the files it attached; if your owner declines, or doesn't answer in
time, that is posted instead. A turn may ask for three private tasks, and
only a few may wait for your owner or run at once; past that the request is
refused until some are done. Refused inside a private task.

## Inside a private task

A private task runs with your owner's resources on your owner's behalf.
There, only `agentctl attach` works; every other command is refused.
