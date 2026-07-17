# Production deployment with Podman + Quadlet (systemd).
#
# Quadlet turns declarative `.container` / `.network` / `.volume` files into
# systemd units, so the stack is managed with `systemctl` (restart, journald
# logging, boot ordering, dependencies) — no separate orchestrator.
#
# Install (rootful, system-wide):
#   sudo cp deploy/quadlet/mda.*.container deploy/quadlet/mda.network deploy/quadlet/mda-postgres.volume \
#        /etc/containers/systemd/
#   sudo cp /etc/mda/mda-app.env.example /etc/mda/mda-app.env && sudoedit /etc/mda/mda-app.env
#   sudo systemctl daemon-reload
#   sudo systemctl enable --now mda-app        # pulls in postgres + redis via Requires=
#
# Rootless (per-user, no root daemon): place the files in
#   ~/.config/containers/systemd/   and drop the `sudo`. Note: rootless + the
# postgres volume can hit UID-mapping permission errors on first init; rootful
# is simpler for a multi-tier prod stack.
#
# The app container:
#  - runs in-process migrations as the owner (DATABASE_URL = mda), which creates
#    the non-superuser mda_app role (migrations/...rls.sql);
#  - serves requests through MDA_APP_DATABASE_URL (mda_app) so biz.* RLS engages.
# Secrets (MDA_APP_DATABASE_URL password, MDA_JWT_SECRET) live in
# /etc/mda/mda-app.env (chmod 600, NOT committed).
