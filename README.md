# radicle-portable / radp

_Safely allows you to use a radicle node that stores on a portable device!_
**Yes this means one radicle identity useable on multiple devices.** :3


## Why
I previously used a lil btrfs portable drive, because well, exfat doesn't support unix domain sockets... However, I might
just end up using something that doesn't support btrfs and be stuck unable to use my node.

Which is why!

## How
Very simple bimple, it creates a temporary radicle folder on the system drive, symlinks the "normal" files from there, 
excluding temporary files (files that can safely stay on disk anyway). This allows for the UDS to remain on the system disk
and the persistent data to be stored away to your external drive. It'll set the fish universal variable `RAD_HOME` so your 
shell points to it. If you're not using Fish, well it's not unsolvable, but it would probably have to write a .profile file or
similar instead, which it doesn't right now. Feel free to PR, but I don't plan on doing it myself.

When _radp_ notices your node is down (`rad node stop`) it'll automatically remove the symlinks and clean up the files, then
if it exists, start your local radicle node, usually the one in `~/.radicle`. You won't have to do a thing!
You can also just hit `[ENTER]` if rad-portable is running in a terminal window. Gives you more overview anyways.

## What you need
Fish, and obviously radicle.
