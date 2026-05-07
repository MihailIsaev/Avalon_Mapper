# Avalon Sync Server

Small HTTP sync server for shared Avalon Mapper edges.

## VPS setup

Copy `avalon_sync_server.py` to the VPS:

```bash
sudo mkdir -p /opt/avalon-sync /var/lib/avalon-sync
sudo cp avalon_sync_server.py /opt/avalon-sync/
sudo chmod +x /opt/avalon-sync/avalon_sync_server.py
sudo chown -R www-data:www-data /var/lib/avalon-sync
```

Create a write token:

```bash
openssl rand -hex 24
```

Install the systemd unit:

```bash
sudo cp avalon-sync.service /etc/systemd/system/avalon-sync.service
sudo systemctl edit avalon-sync
```

Add the token in the editor:

```ini
[Service]
Environment=AVALON_SYNC_WRITE_TOKEN=paste-token-here
```

Start the service:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now avalon-sync
sudo systemctl status avalon-sync
```

Install nginx proxy:

```bash
sudo cp nginx-avalon-sync.conf /etc/nginx/sites-available/avalon-sync
sudo ln -s /etc/nginx/sites-available/avalon-sync /etc/nginx/sites-enabled/avalon-sync
sudo nginx -t
sudo systemctl reload nginx
```

In the app Settings page, enable sync and use:

```text
http://your-vps-ip/sync
```

Use the same write token on every client.
