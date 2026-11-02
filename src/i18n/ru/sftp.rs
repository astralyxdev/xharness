//! Russian for SFTP windows and their server list (see `ru/mod.rs`).

pub const ENTRIES: &[(&str, &str)] = &[
    // The mod
    ("SFTP", "SFTP"),
    (
        "Your servers' files next to the project's, like FileZilla: add any number of servers, drag files across, watch the transfers.",
        "Файлы ваших серверов рядом с файлами проекта, как в FileZilla: добавляйте сколько угодно серверов, перетаскивайте файлы, следите за передачей.",
    ),
    ("New SFTP window", "Новое окно SFTP"),
    // Toolbar and status
    ("No servers yet", "Серверов пока нет"),
    ("Pick a server", "Выберите сервер"),
    ("Connect", "Подключиться"),
    ("Disconnect", "Отключиться"),
    ("Connected to {}", "Подключено к {}"),
    ("Not connected", "Не подключено"),
    ("Waiting for you to trust the server", "Ждёт, пока вы доверитесь серверу"),
    ("Add a server…", "Добавить сервер…"),
    ("Manage servers…", "Управление серверами…"),
    // The two sides
    ("A folder on the server: /var/www, ~/logs", "Папка на сервере: /var/www, ~/logs"),
    ("Server", "Сервер"),
    ("Permissions", "Права"),
    ("New folder", "Новая папка"),
    ("New name", "Новое имя"),
    ("Folder name", "Имя папки"),
    ("Create", "Создать"),
    ("Upload “{}”", "Отправить «{}»"),
    ("Download “{}”", "Скачать «{}»"),
    ("Connect to a server first.", "Сначала подключитесь к серверу."),
    (
        "Files from this Mac can't be sent from a remote project: drag them from its Explorer instead.",
        "Файлы с этого Mac нельзя отправить из удалённого проекта: перетащите их из его Проводника.",
    ),
    ("Delete “{}” from the server?", "Удалить «{}» с сервера?"),
    ("Folders go with everything in them. This can't be undone.", "Папки удаляются со всем содержимым. Это нельзя отменить."),
    ("This can't be undone.", "Это нельзя отменить."),
    // Connecting
    (
        "Add the servers you send files to: the address, the user, and how to sign in. Every project's SFTP windows can use them.",
        "Добавьте серверы, на которые отправляете файлы: адрес, пользователя и способ входа. Ими смогут пользоваться окна SFTP любого проекта.",
    ),
    ("Add a server", "Добавить сервер"),
    ("Pick a server to connect to", "Выберите сервер для подключения"),
    ("First time connecting to {}", "Первое подключение к {}"),
    ("Make sure this is its key before you trust it. Its fingerprint:", "Прежде чем доверять, убедитесь, что это его ключ. Отпечаток:"),
    (
        "Trusting it adds it to ~/.ssh/known_hosts, so you won't be asked again.",
        "Ключ будет добавлен в ~/.ssh/known_hosts, и больше вас не спросят.",
    ),
    ("Trust and connect", "Доверять и подключиться"),
    ("Couldn't connect", "Не удалось подключиться"),
    ("Edit server", "Изменить сервер"),
    ("The connection was closed.", "Соединение закрыто."),
    (
        "{}'s key has changed (it's now {}). Someone may be listening in, or the server was set up again. If you expected it, remove its old key from ~/.ssh/known_hosts and connect again.",
        "Ключ {} изменился (теперь {}). Возможно, соединение перехватывают, или сервер переустановили. Если вы этого ждали, удалите старый ключ из ~/.ssh/known_hosts и подключитесь снова.",
    ),
    // Transfers
    (
        "Drag files from one side to the other to copy them, or double-click a file to send it across.",
        "Перетащите файлы с одной стороны на другую, чтобы скопировать, или дважды щёлкните файл, чтобы отправить его.",
    ),
    ("Clear finished", "Убрать завершённые"),
    ("{} and {} more", "{} и ещё {}"),
    ("Getting ready…", "Подготовка…"),
    ("{} of {} files · {} of {}", "{} из {} файлов · {} из {}"),
    ("Cancelled", "Отменено"),
    // The server list
    ("Servers", "Серверы"),
    ("New server", "Новый сервер"),
    ("Production", "Продакшн"),
    ("Host", "Хост"),
    ("Port", "Порт"),
    ("User", "Пользователь"),
    ("Sign in with", "Вход"),
    ("SSH keys", "SSH-ключи"),
    ("Password", "Пароль"),
    ("Key file", "Файл ключа"),
    ("Passphrase", "Парольная фраза"),
    ("None", "Нет"),
    ("Folder to open", "Открывать папку"),
    (
        "The SSH agent's keys, then ~/.ssh/id_ed25519, id_ecdsa or id_rsa.",
        "Ключи SSH-агента, затем ~/.ssh/id_ed25519, id_ecdsa или id_rsa.",
    ),
    (
        "The SSH agent's keys, then ~/.ssh/id_ed25519, id_ecdsa or id_rsa, on this project's server (it's what connects).",
        "Ключи SSH-агента, затем ~/.ssh/id_ed25519, id_ecdsa или id_rsa на сервере этого проекта (подключается он).",
    ),
    ("Saved on this Mac in a file only you can read.", "Хранится на этом Mac в файле, который можете прочитать только вы."),
    (
        "The key file is looked for on this project's server (it's what connects).",
        "Файл ключа ищется на сервере этого проекта (подключается он).",
    ),
    ("Type the server's address.", "Введите адрес сервера."),
    ("Type the user name to sign in with.", "Введите имя пользователя для входа."),
    ("The port is a number from 1 to 65535.", "Порт — число от 1 до 65535."),
    ("Pick the key file to sign in with.", "Укажите файл ключа для входа."),
    ("Save and connect", "Сохранить и подключиться"),
    ("Close", "Закрыть"),
    ("Remove", "Удалить"),
    ("Remove “{}”?", "Удалить «{}»?"),
    (
        "It goes from the server list of every project. Files on the server stay.",
        "Сервер исчезнет из списка во всех проектах. Файлы на сервере останутся.",
    ),
];

pub const PLURALS: &[(&str, [&str; 3])] = &[
    ("Upload {} item", ["Отправить {} объект", "Отправить {} объекта", "Отправить {} объектов"]),
    ("Download {} item", ["Скачать {} объект", "Скачать {} объекта", "Скачать {} объектов"]),
    ("Delete {} item from the server?", ["Удалить {} объект с сервера?", "Удалить {} объекта с сервера?", "Удалить {} объектов с сервера?"]),
    ("{} transfer", ["{} передача", "{} передачи", "{} передач"]),
    ("{} file copied", ["{} файл скопирован", "{} файла скопировано", "{} файлов скопировано"]),
];
