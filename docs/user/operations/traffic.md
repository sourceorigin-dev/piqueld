# Seeing who visits an application

Find out how many people use an application, which pages they open, and which
sites send them, without adding analytics to the application or running a
telemetry stack.

piqueld's [gateway](../networking/routes.md) counts the requests each route
serves. It keeps hourly totals, not the requests themselves, visitors'
addresses or cookies; see [what is counted](#what-is-counted).

## Before you start

- The gateway is enabled, with
  [`ingress.enabled`](../reference/configuration-settings.md#ingressenabled),
  and the application has a [route](../networking/routes.md#add-a-route) to
  one of its services. Public and [private](../networking/private-routes.md)
  routes are both counted; [redirects](../networking/routes.md#redirect-a-hostname) aren't.
- Reading traffic needs the `logs:read`
  [permission](../accounts/permissions.md#grants) on the application.

piqueld counts traffic by default. To turn it off, set
[`ingress.traffic`](../reference/configuration-settings.md#ingresstraffic) to
`false` and restart the daemon: on NixOS,
`services.piqueld.settings.ingress.traffic = false;`, or in
`/etc/piqueld/config.toml`:

```toml
[ingress]
enabled = true
traffic = false
```

## See an environment's traffic

In the dashboard, open the environment's page and select the **Traffic** tab.
Choose the **Period** and, to see one service's routes only, the **Service**.

- The tiles show the **Visitors a day** on average, from the period's first
  day (or hour) with requests, with their sum, and the period's **Page views**, **Requests** with
  their server errors, and **Mean response** time.
- The chart shows visitors per hour for a period of 24 hours, and per day for
  longer ones. Choose page views or requests in **Show**. Point at a column,
  or focus it with the keyboard, to read all its counts.
- **Top pages** and **Referring sites** list the 20 most viewed pages and the
  other sites whose links led to them.
- **Routes** lists each route's totals when the environment has several.

The tab reads the counts again every minute.

With `piquelctl`:

```console
$ piquelctl env traffic sc2dle production --since 3d
Environment: sc2dle / production
Window: 2026-11-13 to 2026-11-16 UTC
Visitors a day: 475 on average (1707 over the window)
Page views: 3677
Requests: 27504 (350 client errors, 4 server errors)
Mean response: 15 ms
Sent: 367.2 MiB

DAY         VISITORS  PAGE VIEWS  REQUESTS  SERVER ERRORS
2026-11-13  531       1146        8542      1
2026-11-14  526       1115        8334      1
…
```

The table has a row per day, or per hour for windows of up to two days.
Tables of the routes, pages and referring sites follow. Times are in UTC. `--since` and
`--until` take durations such as `24h` or `30d`; by default, the command
shows the last 30 days. `--service` and `--hostname` narrow it to some
routes. Add `--json` for the full report.

## What is counted

- **Requests** are every request a route sends to its service, bots'
  included, with how many failed with a 4xx or 5xx status, the bytes sent and
  the time to answer.
- **Page views** are successful `GET` requests from browsers for a page:
  those that load a document, as browsers say with `Sec-Fetch-Dest`, or that
  are answered with HTML. Scripts, images, API calls, and pages a browser
  only prefetches aren't page views.
- **Visitors** are the distinct browsers with a page view on a route each UTC
  day, told apart by their address and user agent. A visitor coming back
  another day counts again, and several people behind one address with the
  same browser count once.
- **Top pages** count page views by path, without the query string. **Referring
  sites** count them by the hostname of the `Referer` header, unless it is the
  route's own. Both are counted per UTC day, so they cover the whole days a
  period touches.

Requests from bots, crawlers, scripts such as `curl`, and uptime monitors
count as requests, but never as page views or visitors. The gateway's own
health checks aren't counted.

Counts are kept for
[`retention.traffic_days`](../reference/configuration-settings.md#retentiontraffic_days),
90 by default. Deleting an environment deletes its counts.

While the daemon restarts, the gateway can't hand it requests, so it writes
them, addresses and paths included, to its own container log instead, for up
to 10 seconds after the daemon is back. Docker keeps that log, capped at
30 MB, like the gateway's other output.

## Troubleshooting

- **The tab says piqueld isn't counting traffic.** Set `ingress.enabled` and
  `ingress.traffic` in the daemon's configuration, then restart it. Counts
  recorded before stay readable.
- **Nothing appears.** New requests appear within a minute. Only routes to a
  service are counted: check the environment's **URLs** card, or
  `piquelctl env url`, for its routes.
- **A single-page application shows few page views.** Page views count pages
  the browser loads. Navigating within an application that changes pages
  without loading one isn't a page view; its API calls still count as
  requests.
- **Visitors over a period add up to more people than use the application.**
  A visitor counts once a day, so someone coming back every day counts every
  day. **Visitors a day** shows how many people visit on an average day.
- **Visitors seem high after a restart.** The daemon remembers the day's
  visitors in memory only, so those coming back after it restarts count again
  that day.
- **Some pages are missing from Top pages.** piqueld counts up to 100
  different pages, and 100 referring sites, per route and day. Pages first
  seen after that aren't counted for the day.
