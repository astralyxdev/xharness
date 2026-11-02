//! Russian for the canvas: its tools, marks and menus (see `ru/mod.rs`).

pub const ENTRIES: &[(&str, &str)] = &[
    ("Nothing here yet", "Здесь пока пусто"),
    (
        "Reopen a window from the sidebar, start a terminal, or mark the canvas up with the tools below.",
        "Откройте окно из боковой панели, запустите терминал или разметьте холст инструментами внизу.",
    ),
    (
        "Windows, notes and sections sit anywhere on this canvas. Drag empty space to pan, ⌘-scroll or pinch to zoom.",
        "Окна, заметки и разделы можно размещать где угодно на этом холсте. Тяните пустое место, чтобы двигать холст; ⌘ с прокруткой или щипок — масштаб.",
    ),
    // Tools
    ("Select", "Выделение"),
    ("Text", "Текст"),
    ("Note", "Заметка"),
    ("Section", "Раздел"),
    ("Tidy up windows", "Упорядочить окна"),
    ("New window", "Новое окно"),
    // Marks
    ("Type something", "Введите текст"),
    ("Write a note…", "Напишите заметку…"),
    ("Section name", "Название раздела"),
    ("Double-click to write", "Дважды щёлкните, чтобы писать"),
    ("Edit text", "Изменить текст"),
    ("Duplicate", "Дублировать"),
    ("Delete", "Удалить"),
    ("Add text here", "Добавить текст здесь"),
    ("Add a note here", "Добавить заметку здесь"),
    ("Add a section here", "Добавить раздел здесь"),
    ("New terminal here", "Новый терминал здесь"),
];

pub const PLURALS: &[(&str, [&str; 3])] = &[
];
