// Notifications never cache transcript data or device credentials.
// Take over open pages right after the first activation, so a page that
// registered this worker can already receive clicks through it.
self.addEventListener("activate", event => event.waitUntil(self.clients.claim()));

self.addEventListener("push", event => {
  if (!event.data) return;
  const { title, body, tag, tab } = event.data.json();
  event.waitUntil(self.registration.showNotification(title, {
    body, tag, data: { tab },
  }));
});

self.addEventListener("notificationclick", event => {
  event.notification.close();
  const url = "/?tab=" + encodeURIComponent(event.notification.data.tab);
  event.waitUntil((async () => {
    const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    // navigate() rejects for clients this worker does not control.
    try {
      if (!windows.length) throw new Error("no window");
      await windows[0].navigate(url);
      await windows[0].focus();
    } catch {
      await self.clients.openWindow(url);
    }
  })());
});
