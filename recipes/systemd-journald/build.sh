#!/usr/bin/env bash
# Builds only systemd-journald, journalctl and libsystemd-shared.
# Meson component toggles follow conda-forge/systemd-feedstock, but journald stays on and
# every optional library is disabled except zstd (default journal compression, dlopen()ed).
set -euxo pipefail

meson_config_args=(
    -Dmode=release
    -Dsplit-bin=false
    -Dsysconfdir=etc
    -Dversion-tag=conda
    # Optional dependencies: off unless listed explicitly below.
    -Dauto_features=disabled
    -Dzstd=enabled
    # general toggles
    -Dcreate-log-dirs=false
    -Dinstall-sysconfdir=false
    -Dtests=false
    -Dman=disabled
    -Dhtml=disabled
    # component toggles
    -Danalyze=false
    -Dbacklight=false
    -Dbinfmt=false
    -Dcoredump=false
    -Defi=false
    -Denvironment-d=false
    -Dfirstboot=false
    -Dhibernate=false
    -Dhostnamed=false
    -Dhwdb=false
    -Dinitrd=false
    -Dkernel-install=false
    -Dlocaled=false
    -Dlogind=false
    -Dmachined=false
    -Dmountfsd=false
    -Dnetworkd=false
    -Dnsresourced=false
    -Dnss-myhostname=false
    -Dnss-systemd=false
    -Doomd=false
    -Dportabled=false
    -Dpstore=false
    -Dquotacheck=false
    -Drandomseed=false
    -Dresolve=false
    -Drfkill=false
    -Dsysext=false
    -Dsysusers=false
    -Dtimedated=false
    -Dtimesyncd=false
    -Dtmpfiles=false
    -Duserdb=false
    -Dvconsole=false
    -Dxdg-autostart=false
)

meson setup forgebuild \
    ${MESON_ARGS} \
    "${meson_config_args[@]}" \
    --wrap-mode=nofallback

shared_lib="libsystemd-shared-${PKG_VERSION%%.*}.so"

meson compile -v -C forgebuild -j "${CPU_COUNT}" systemd-journald journalctl

# Install by hand: `meson install` would require building every target. Build-tree RPATHs
# point into forgebuild/src/shared, so set the install-time ones (what `meson install` would
# write); rattler-build then rewrites these absolute prefix paths to $ORIGIN-relative ones.
install -Dm755 forgebuild/systemd-journald "${PREFIX}/lib/systemd/systemd-journald"
install -Dm755 forgebuild/journalctl "${PREFIX}/bin/journalctl"
install -Dm755 "forgebuild/src/shared/${shared_lib}" "${PREFIX}/lib/systemd/${shared_lib}"

patchelf --set-rpath "${PREFIX}/lib/systemd:${PREFIX}/lib" \
    "${PREFIX}/lib/systemd/systemd-journald" "${PREFIX}/bin/journalctl"
patchelf --set-rpath "${PREFIX}/lib" "${PREFIX}/lib/systemd/${shared_lib}"
