set -e
tasks=(
    '-p pubsub'
    '-p pubsub --features serenity_cache'
    '-p json-db'
    '-p daemons'
    '-p daemons --features cron'
    '--workspace'
)

for task in "${tasks[@]}"; do
    cargo clean
    IFS=' ' read -r -a args <<< "$task"
    printf "compiling ${args[*]} ... "
    cargo --quiet check "${args[@]}"
    printf "\e[32mOK\e[0m\n"
done
