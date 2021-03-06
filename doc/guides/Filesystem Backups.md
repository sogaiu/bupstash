# Filesystem Backups

This guide will cover how to use bupstash for system backups, it is divided into
sections which cover different use cases.

For all of the guides the shown commands can be put into a cron job or other tool for running background tasks
for automated backups.

The guides below can also be combined with remote repositories with access controls to allow 'upload only' for secure deployments.

## Simple directory snapshots

The simplest use of bupstash is to simply snapshot your home directory to a repository on an external drive.

Create the file backup.sh:

```
set -eu
export BUPSTASH_KEY=/root/backup-put.key
export BUPSTASH_REPOSITORY=/mnt/external-drive/bupstash-backups

bupstash put \
   --send-log /root/backup.sendlog \
   --exclude "/home/*/.cache" \
   hostname=$(hostname) \
   name=home-backup.tar \
   /home/
```

Then running a backup is as simple as:

```
$ sudo sh ./backup.sh
```

Now to restore files or sub directories we can use `bupstash get`:

```
$ bupstash list name=home-backup.tar
...
$ bupstash get id=$backupid | tar -C restore ...
$ bupstash get --pick some/sub-dir id=$backupid | tar -C restore ...
$ bupstash get --pick some/file.txt id=$backupid > file.txt
```

Some points to consider about this snapshot method:

- The use of --exclude to omit the user cache directories, we can save a lot of space in backups by ignoring things
  like out web browser cache, at the expense of less complete backups. You can specify --exclude more than once to
  skip more than one directory or file.

- The use of an explicit `--send-log` option ensures bupstash will be able to perform incremental backups efficiently. Bupstash
  incremental backups work best when you use a different send log for each different backup operation.

- This method of backup is simple, but does not account for files being modified during upload. If a file were to be written to while a backup was taking 
  place,  The simplest way to to think about this problem, is files will be changing while the backup is uploading, so you might capture different directories at different points in time.

- In this command we are also using a 'put' key (see the offline keys guide) so that backups cannot be decrypted even if someone was to steal your external drive.


## Btrfs directory snapshots

If you are running linux with btrfs, (or any other operating system + filesystem that supports snapshots), you can
use this to get good snapshots with no 'time smear'.


Create the file backup.sh:

```
set -eu
export BUPSTASH_KEY=/root/backup-put.key
export BUPSTASH_REPOSITORY=/mnt/external-drive/bupstash-backups


if test -e /rootsnap
then
    echo "removing snapshot, it already existed."
    btrfs subvolume delete /rootsnap
fi
btrfs subvolume snapshot -r / /rootsnap > /dev/null

bupstash put \
   --send-log /root/backup.sendlog \
   --exclude "/home/*/.cache" \
   hostname=$(hostname) \
   name=backup.tar \
   /rootsnap

btrfs subvolume delete /rootsnap > /dev/null
```

Then running a backup is as simple as:

```
$ sudo sh ./backup.sh
```

Filesystem enabled snapshots do not suffer from 'time smear'. All points about '--send-log', '--exclude' and backup restore from simple directory snapshots also apply to this snapshot method.


## Btrfs send snapshots


If you are running linux with btrfs, (or any other operating system + filesystem that supports exporting directories as a stream), you can
directly save the output of such a command into a bupstash repository.


Create the file backup.sh:

```
set -eu
export BUPSTASH_KEY=/root/backup-put.key
export BUPSTASH_REPOSITORY=/mnt/external-drive/bupstash-backups


if test -e /rootsnap
then
    echo "removing snapshot, it already existed."
    btrfs subvolume delete /rootsnap
fi

btrfs subvolume snapshot -r / /rootsnap > /dev/null

bupstash put \
   --exec
   --send-log /root/backup.sendlog \
   hostname=$(hostname) \
   name=backup.btrfs \
   btrfs send  /rootsnap

btrfs subvolume delete /rootsnap > /dev/null
```
Then running a backup is as simple as:

```
$ sudo sh ./backup.sh
```

Restoration of the backup is done via the `btrfs receive` command:

```
$ bupstash get id=$backupid | sudo btrfs receive  ./restore
```