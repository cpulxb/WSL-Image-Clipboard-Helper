# Runs inside one non-interactive SSH channel. No daemon or installation on the server.
set -eu
umask 077
token=$1
dir=$(mktemp -d "/tmp/wsl_clipboard-$(id -u)-XXXXXXXXXX")
cleanup() {
    rm -f -- "$dir"/.partial "$dir"/t*
    rmdir -- "$dir" 2>/dev/null || :
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
command -v head >/dev/null
command -v wc >/dev/null
printf '%s READY\n' "$token"
counter=0
while IFS=' ' read -r op size kind name; do
    case "$op" in
        PING) printf '%s PONG\n' "$token" ;;
        QUIT) exit 0 ;;
        PUT)
            case "$size" in ''|*[!0-9]*) exit 2 ;; esac
            case "$kind" in t|f) ;; *) exit 2 ;; esac
            name=$(printf '%b' "$name")
            case "$name" in ''|.|..|*/*) exit 2 ;; esac
            counter=$((counter + 1))
            file="$dir/$kind${counter}_$name"
            : > "$dir/.partial"
            printf '%s SEND\n' "$token"
            head -c "$size" > "$dir/.partial"
            [ "$(wc -c < "$dir/.partial")" -eq "$size" ] || exit 3
            mv -- "$dir/.partial" "$file"
            printf '%s OK %s\n' "$token" "$file"
            ;;
        *) exit 2 ;;
    esac
done
