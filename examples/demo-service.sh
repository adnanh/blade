#!/bin/sh

trap 'exit 0' INT TERM

case "$1" in
    postgres)
        printf "Starting PostgreSQL 17...\ndatabase system is ready to accept connections\n"
        while :; do sleep 1; done
        ;;
    redis)
        printf "Starting Redis...\nReady to accept connections on port 6379\n"
        while :; do sleep 1; done
        ;;
    api)
        printf "Loading routes...\nAPI listening on http://127.0.0.1:4000\n"
        request=1
        while :; do
            printf "GET /api/health 200 (%d ms)\n" "$((request % 7 + 2))"
            request=$((request + 1))
            sleep 2
        done
        ;;
    worker)
        printf "Worker connected to Redis\nWatching the default queue\n"
        job=41
        while :; do
            printf "Processed job #%d\n" "$job"
            job=$((job + 1))
            sleep 3
        done
        ;;
    dashboard)
        printf "VITE ready in 412 ms\nLocal: http://localhost:5173/\n"
        update=1
        while :; do
            printf "hmr update /src/views/Dashboard.tsx (%d)\n" "$update"
            update=$((update + 1))
            sleep 2
        done
        ;;
    storybook)
        printf "Storybook 9.0 started\nLocal: http://localhost:6006/\n"
        while :; do sleep 1; done
        ;;
    git-pull)
        printf "$ git pull --ff-only\nAlready up to date.\n"
        ;;
    *)
        printf "Unknown demo service: %s\n" "$1" >&2
        exit 2
        ;;
esac
