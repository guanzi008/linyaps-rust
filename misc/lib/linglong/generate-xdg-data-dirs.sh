if [ -x /usr/libexec/linglong/ll-system-helper ]; then
    XDG_DATA_DIRS="$(/usr/libexec/linglong/ll-system-helper xdg-value)"
fi
