#!/bin/sh
# Two actual-product examples from the committed tour. No new runtime capture.
set -eu
cd "$(dirname "$0")/.."
source_video=public/video/vibeke-tour.mp4
ffmpeg -hide_banner -loglevel error -i "$source_video" -filter_complex '[0:v]trim=start=22.6:end=29.6,setpts=PTS-STARTPTS,scale=1280:720,setsar=1[a];[0:v]trim=start=115.5:end=128.5,setpts=PTS-STARTPTS,scale=1280:720,setsar=1[b];[a][b]concat=n=2:v=1:a=0[v]' -map '[v]' -an -c:v libx264 -preset fast -crf 24 -pix_fmt yuv420p -movflags +faststart public/video/quick-look.mp4 -y
ffmpeg -hide_banner -loglevel error -i "$source_video" -filter_complex '[0:v]trim=start=22.6:end=29.6,setpts=PTS-STARTPTS,crop=800:800:820:135,scale=720:720,setsar=1[a];[0:v]trim=start=115.5:end=121.5,setpts=PTS-STARTPTS,crop=470:470:1290:130,scale=720:720,setsar=1[b];[0:v]trim=start=121.5:end=128.5,setpts=PTS-STARTPTS,crop=470:470:1290:470,scale=720:720,setsar=1[c];[a][b][c]concat=n=3:v=1:a=0[v]' -map '[v]' -an -c:v libx264 -preset fast -crf 24 -pix_fmt yuv420p -movflags +faststart public/video/quick-look-mobile.mp4 -y
ffmpeg -hide_banner -loglevel error -ss 8.5 -i public/video/quick-look.mp4 -frames:v 1 public/video/quick-look-poster.jpg -y
ffmpeg -hide_banner -loglevel error -ss 8.5 -i public/video/quick-look-mobile.mp4 -frames:v 1 public/video/quick-look-mobile-poster.jpg -y
