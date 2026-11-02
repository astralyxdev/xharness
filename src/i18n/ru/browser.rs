//! Russian for Browser windows (`windows/browser/`, `platform/webview.rs`).

pub const ENTRIES: &[(&str, &str)] = &[
    // The toolbar and the address bar.
    ("Search, or type an address: localhost:3000, example.com", "Поиск или адрес: localhost:3000, example.com"),
    ("Not secure", "Не защищено"),
    ("can't open {}", "не открывается {}"),
    // The ⋯ menu.
    ("Open in default browser", "Открыть в браузере по умолчанию"),
    ("Copy address", "Скопировать адрес"),
    ("Address copied", "Адрес скопирован"),
    ("Reload ignoring cache", "Перезагрузить без кэша"),
    ("Zoom in", "Увеличить"),
    ("Zoom out", "Уменьшить"),
    ("Actual size", "Реальный размер"),
    ("Actual size (now {}%)", "Реальный размер (сейчас {}%)"),
    ("Find in page…", "Найти на странице…"),
    ("Copy screenshot", "Скопировать снимок экрана"),
    ("Screenshot copied", "Снимок скопирован"),
    ("Show console", "Показать консоль"),
    ("Hide console", "Скрыть консоль"),
    ("Show Web Inspector", "Открыть Web Inspector"),
    // Find in page.
    ("Find in page", "Найти на странице"),
    ("No matches", "Совпадений нет"),
    ("Finding text needs macOS 13 or later.", "Поиск по тексту работает в macOS 13 и новее."),
    // Sizes.
    ("Responsive", "Адаптивный"),
    ("Responsive (the window's size)", "Адаптивный (размер окна)"),
    // The console.
    ("Console", "Консоль"),
    ("Nothing logged yet.", "Пока ничего не выведено."),
    // The start page, and pages that don't open.
    ("Open a page", "Откройте страницу"),
    ("Type an address above, or a dev server's port, like 3000.", "Введите адрес выше или порт dev-сервера, например 3000."),
    ("Recently visited", "Недавно открытые"),
    ("Can't open this page", "Не удаётся открыть страницу"),
    ("Try again", "Повторить"),
    ("That isn't a web address.", "Это не веб-адрес."),
    ("Pages show on macOS for now", "Пока страницы показываются только в macOS"),
    ("Open this one in your browser instead.", "Откройте её в своём браузере."),
    ("Open in browser", "Открыть в браузере"),
];

pub const PLURALS: &[(&str, [&str; 3])] = &[
    ("{} message", ["{} сообщение", "{} сообщения", "{} сообщений"]),
    ("{} error", ["{} ошибка", "{} ошибки", "{} ошибок"]),
];
