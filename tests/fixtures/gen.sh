#!/usr/bin/env bash
# Generates the test fixtures and asserts, with ffprobe, that each has its intended defect.
#
# Usage: gen.sh          regenerate every fixture, then check them
#        gen.sh --check  check the committed fixtures only
#
# The checks read timestamps and chapters straight from ffprobe rather than through vt, so a
# fixture that quietly lost its defect fails here instead of producing a passing test.

set -euo pipefail
cd "$(dirname "$0")"

FPS=60
SECS=10

generate() {
    local video=(-f lavfi -i "testsrc2=size=160x90:rate=$FPS:duration=$SECS")
    local audio=(-f lavfi -i "sine=frequency=440:sample_rate=48000:duration=$SECS")
    # B-frames make decode order differ from presentation order, as in real recordings.
    local encode=(-c:v libx264 -preset veryfast -g "$FPS" -bf 2 -pix_fmt yuv420p
        -c:a aac -b:a 64k -video_track_timescale 90000 -map_metadata -1 -fflags +bitexact)
    local ff=(ffmpeg -v error -y)

    "${ff[@]}" "${video[@]}" "${audio[@]}" "${encode[@]}" clean-cfr.mp4

    # Matroska's 1 ms time base stores 16.67 ms frames as a mix of 16 and 17 ms intervals.
    "${ff[@]}" -i clean-cfr.mp4 -c copy -fflags +bitexact clean-cfr.mkv

    # Jitters each frame by up to +-0.67 ms around its slot (60 ticks of 1/90000 s), like the
    # NVIDIA recordings. Every frame stays, so the average rate is exact. The jitter keys off
    # the presentation slot (PTS/1500) and is zero for the first frame, and DTS drops by the
    # largest jitter so it stays monotonic and never exceeds PTS.
    "${ff[@]}" -i clean-cfr.mp4 -c copy -fflags +bitexact \
        -bsf:v "setts=pts=PTS+(mod(PTS/1500*7+2\,5)-2)*30:dts=DTS-60" vfr-jitter.mp4

    # Drops frame 100 alone and frames 250-251 together, keeping the survivors' timestamps.
    "${ff[@]}" "${video[@]}" "${audio[@]}" \
        -vf "select=not(eq(n\,100)+eq(n\,250)+eq(n\,251))" -fps_mode passthrough \
        "${encode[@]}" dropped-frames.mp4

    # Shifts every audio packet 0.5 s later.
    "${ff[@]}" -i clean-cfr.mp4 -c copy -fflags +bitexact \
        -bsf:a "setts=ts=TS+24000" audio-offset.mp4

    # Drops the audio packets between 4.0 and 4.5 s, leaving a hole in the timeline.
    "${ff[@]}" -i clean-cfr.mp4 -c copy -fflags +bitexact \
        -bsf:a "noise=drop=between(pts\,192000\,215999)" audio-gap.mp4

    # Ends the audio a second before the video, like the AMD recordings.
    "${ff[@]}" "${video[@]}" -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=$((SECS - 1))" \
        "${encode[@]}" audio-short-tail.mp4

    # Three valid chapters. The last is untitled, so vt names it "Chapter 3".
    chaptered chapters-valid.mkv 45 "0 15 Intro" "15 30 Setup" "30 45"

    # Breaks three YouTube rules: it starts at 0:02, has only two chapters, and the last is 5 s.
    chaptered chapters-invalid.mkv 25 "2 20 Late start" "20 25 Blink"

    # Runs past an hour, so timestamps switch to H:MM:SS.
    chaptered chapters-hours.mkv 3660 "0 1800 First half" "1800 3600 Second half" "3600 3660 Credits"

    speech_bursts
}

# Tone bursts at 1-2 s and 3.5-4.5 s in 6 s of silence, which the fake transcriber reads as two
# lines of speech. Lossless audio at the transcriber's 16 kHz keeps the edges exact.
speech_bursts() {
    ffmpeg -v error -y -f lavfi -i "color=black:size=32x32:rate=1:duration=6" \
        -f lavfi -i "aevalsrc=0.5*sin(2*PI*440*t)*(gte(t\,1)*lt(t\,2)+gte(t\,3.5)*lt(t\,4.5)):s=16000:d=6" \
        -c:v libx264 -preset veryfast -c:a flac -map_metadata -1 -fflags +bitexact speech-bursts.mkv
}

# Encodes a tiny, blank, silent video lasting $2 seconds into $1, with chapters given as
# "START END [TITLE]" in seconds.
chaptered() {
    local out=$1 secs=$2 meta start end title
    shift 2
    meta=$(mktemp)
    {
        echo ";FFMETADATA1"
        for chapter in "$@"; do
            read -r start end title <<<"$chapter"
            printf '[CHAPTER]\nTIMEBASE=1/1\nSTART=%s\nEND=%s\n' "$start" "$end"
            if [[ -n $title ]]; then
                printf 'title=%s\n' "$title"
            fi
        done
    } >"$meta"
    ffmpeg -v error -y -f lavfi -i "color=black:size=32x32:rate=1:duration=$secs" -i "$meta" \
        -map 0 -map_metadata 1 -map_chapters 1 -c:v libx264 -preset veryfast -fflags +bitexact "$out"
    rm "$meta"
}

# Prints the duration and each chapter as "START-END [TITLE]", in whole seconds, e.g.
# "45s: 0-15 Intro; 15-30 Setup".
chapter_list() {
    printf '%ds:' "$(ffprobe -v error -of csv=p=0 -show_entries format=duration "$1" | cut -d. -f1)"
    ffprobe -v error -of csv=p=0 -show_entries chapter=start_time,end_time:chapter_tags=title "$1" |
        awk -F, '{ printf "%s %d-%d%s", (NR > 1 ? ";" : ""), $1, $2, ($3 == "" ? "" : " " $3) }'
}

# Prints "count first_pts end_pts deltas..." for one stream's kept packets in presentation
# order, all in ticks. Discarded packets (AAC priming) are skipped.
timeline() {
    ffprobe -v error -select_streams "$2" -of csv=p=0 \
        -show_entries packet=pts,duration,flags "$1" |
        awk -F, '$3 !~ /D/ { print $1, $2 }' | sort -n |
        awk 'NR > 1 { deltas = deltas " " ($1 - prev) } { prev = $1; last = $1 + $2; if (NR == 1) first = $1 }
             END { print NR, first, last deltas }'
}

# Prints the stretches where the first audio stream is louder than -40 dBFS, as "START-END"
# in seconds.
loud_stretches() {
    ffmpeg -v info -nostats -i "$1" -map 0:a:0 -af silencedetect=noise=-40dB:d=0.1 -f null - 2>&1 |
        awk '/silence_start/ && $5 > 0 { out = out sep sprintf("%.3f-%.3f", start, $5); sep = " " }
             /silence_end/ { start = $5 }
             END { print out }'
}

time_base_den() {
    ffprobe -v error -select_streams "$2" -of csv=p=0 -show_entries stream=time_base "$1" | cut -d/ -f2
}

failures=0
fail() {
    echo "FAIL $1: $2" >&2
    failures=$((failures + 1))
}

# Classifies video intervals against the nominal frame duration den/FPS, in integer math:
# regular within 1 tick, dropped at 1.5 frames or more, irregular otherwise.
# Prints "regular irregular drops missing_frames".
video_intervals() {
    local den
    den=$(time_base_den "$1" v:0)
    timeline "$1" v:0 | awk -v den="$den" -v fps="$FPS" '{
        for (i = 4; i <= NF; i++) {
            d = $i * fps
            if (2 * d >= 3 * den) { drops++; missing += int((2 * d + den) / (2 * den)) - 1 }
            else if (d - den <= fps && den - d <= fps) regular++
            else irregular++
        }
        print regular + 0, irregular + 0, drops + 0, missing + 0
    }'
}

# Prints "count total_ticks" for audio intervals over 1.5 packet durations.
audio_gaps() {
    timeline "$1" a:0 | awk '{
        frame = $4
        for (i = 5; i <= NF; i++) if ($i < frame) frame = $i
        for (i = 4; i <= NF; i++) if (2 * $i > 3 * frame) { n++; total += $i - frame }
        print n + 0, total + 0
    }'
}

# Prints audio minus video start and end, in milliseconds.
audio_vs_video_ms() {
    local vden aden
    vden=$(time_base_den "$1" v:0)
    aden=$(time_base_den "$1" a:0)
    paste <(timeline "$1" v:0 | cut -d' ' -f2,3) <(timeline "$1" a:0 | cut -d' ' -f2,3) |
        awk -v vden="$vden" -v aden="$aden" '{
            printf "%d %d\n", ($3 / aden - $1 / vden) * 1000, ($4 / aden - $2 / vden) * 1000
        }'
}

expect() {
    local file=$1 what=$2 actual=$3 expected=$4
    [[ $actual == "$expected" ]] || fail "$file" "$what is '$actual', expected '$expected'"
}

# Asserts a millisecond value lies within [lo, hi].
expect_ms() {
    local file=$1 what=$2 actual=$3 lo=$4 hi=$5
    ((actual >= lo && actual <= hi)) || fail "$file" "$what is ${actual} ms, expected ${lo}..${hi} ms"
}

check() {
    local file intervals gaps offsets
    # Clean files may differ from the video at either end by one AAC frame (21 ms) of
    # priming or padding.
    for file in clean-cfr.mp4 clean-cfr.mkv; do
        read -r _ irregular drops _ <<<"$(video_intervals "$file")"
        expect "$file" "irregular/dropped intervals" "$irregular $drops" "0 0"
        expect "$file" "audio gaps" "$(audio_gaps "$file" | cut -d' ' -f1)" 0
        read -r start end <<<"$(audio_vs_video_ms "$file")"
        expect_ms "$file" "audio start offset" "$start" -21 21
        expect_ms "$file" "audio end offset" "$end" -21 21
    done

    file=vfr-jitter.mp4
    read -r _ irregular drops _ <<<"$(video_intervals "$file")"
    ((irregular > 0)) || fail "$file" "has no irregular intervals"
    expect "$file" "dropped intervals" "$drops" 0
    expect "$file" "frame count" "$(timeline "$file" v:0 | cut -d' ' -f1)" $((FPS * SECS))

    file=dropped-frames.mp4
    read -r _ irregular drops missing <<<"$(video_intervals "$file")"
    expect "$file" "irregular/drops/missing frames" "$irregular $drops $missing" "0 2 3"

    # The shifted priming packet loses its discard flag, so audio starts one AAC frame early.
    file=audio-offset.mp4
    read -r start _ <<<"$(audio_vs_video_ms "$file")"
    expect_ms "$file" "audio start offset" "$start" 478 522

    file=audio-gap.mp4
    read -r count total <<<"$(audio_gaps "$file")"
    expect "$file" "audio gap count" "$count" 1
    expect_ms "$file" "audio gap length" $((total / 48)) 479 521

    file=audio-short-tail.mp4
    read -r start end <<<"$(audio_vs_video_ms "$file")"
    expect_ms "$file" "audio start offset" "$start" -21 21
    expect_ms "$file" "audio end offset" "$end" -1021 -979

    expect chapters-valid.mkv chapters "$(chapter_list chapters-valid.mkv)" \
        "45s: 0-15 Intro; 15-30 Setup; 30-45"
    expect chapters-invalid.mkv chapters "$(chapter_list chapters-invalid.mkv)" \
        "25s: 2-20 Late start; 20-25 Blink"
    expect chapters-hours.mkv chapters "$(chapter_list chapters-hours.mkv)" \
        "3660s: 0-1800 First half; 1800-3600 Second half; 3600-3660 Credits"
    # The chapters tests use this as their file without chapters.
    expect clean-cfr.mp4 chapters "$(chapter_list clean-cfr.mp4)" "10s:"

    file=speech-bursts.mkv
    expect "$file" "audio" "$(ffprobe -v error -of csv=p=0 -select_streams a \
        -show_entries stream=codec_name,sample_rate,channels "$file")" "flac,16000,1"
    expect "$file" "loud stretches" "$(loud_stretches "$file")" "1.000-2.000 3.500-4.500"

    if ((failures > 0)); then
        echo "$failures fixture check(s) failed" >&2
        exit 1
    fi
    echo "All fixtures have their intended defects."
}

case "${1:-}" in
    --check) ;;
    "") generate ;;
    *)
        echo "usage: $0 [--check]" >&2
        exit 2
        ;;
esac
check
