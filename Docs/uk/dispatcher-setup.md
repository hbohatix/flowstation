# Канал диспетчера: налаштування

[English](../dispatcher-setup.md) · **Українська**

Як підключити FlowStation, зібрану з цього форку, до диспетчерської консолі
[flowstation-platform](https://github.com/hbohatix/flowstation-platform).
Що робить канал і як він поводиться: [dispatcher-link.md](dispatcher-link.md).

Виберіть свій випадок:

| Випадок | Що читати |
|---|---|
| Платформа і **тестова** станція на власному Raspberry Pi станції (робоча станція не зачіпається) | flowstation-platform [`deploy/pi/README.uk.md`](https://github.com/hbohatix/flowstation-platform/blob/main/deploy/pi/README.uk.md) — там збирається цей форк і конфігурація пишеться за вас |
| Станція підключається до платформи **деінде** (ПК/сервер з Docker, інший Pi) | ця сторінка |

Платформа вже має працювати, і ви маєте могти увійти в її консоль
(flowstation-platform [`docs/uk/getting-started.md`](https://github.com/hbohatix/flowstation-platform/blob/main/docs/uk/getting-started.md), кроки 1–3).

## 1. Збірка станції

На Raspberry Pi станції (Rust 1.88+, `libsoapysdr-dev`, `pkg-config`, `cmake`):

```bash
git clone -b feature/dispatcher-link https://github.com/hbohatix/flowstation.git
cd flowstation
cargo build --release --bin bluestation-bs
```

Триває близько 5 хвилин на Pi 4 після першого завантаження crates. Бінарник —
`target/release/bluestation-bs`. З мостом SIP/Asterisk збирайте з `--features asterisk`,
як в основному README; канал диспетчера є в обох збірках.

Якщо станція вже працює з іншої збірки, збережіть її бінарник, щоб можна було повернутися:

```bash
cp /path/to/running/bluestation-bs /path/to/running/bluestation-bs.prev
```

## 2. Реєстрація станції в консолі

У консолі *Базові станції → Додати станцію*: *ID станції* (напр. `station-01`), назва,
*Зареєструвати*. Консоль **один раз** покаже **токен**, готовий **фрагмент `[dispatcher]`** і посилання
на **сертифікат ядра** `core-cert.pem`.

Скопіюйте сертифікат на станцію:

```bash
sudo mkdir -p /etc/flowstation
sudo cp core-cert.pem /etc/flowstation/core-cert.pem
```

## 3. Конфігурація

Вставте фрагмент у кінець `config.toml` станції і перевірте його:

```toml
[dispatcher]
enabled = true
server = "192.168.1.10:4433"                 # ядро, як його досягає ця станція (UDP)
server_name = "localhost"                    # ім'я в сертифікаті ядра
ca_cert = "/etc/flowstation/core-cert.pem"   # завантажений сертифікат
node_id = "station-01"                       # як зареєстровано
token = "…"                                  # з консолі, показується один раз
gssis = [10]                                 # групи диспетчера — лише локальні
fallback_issi = 9000001                      # ISSI того, хто викликає, доки консоль його не задасть
# local_groups = true                        # типово: дозволені також усі лише локальні групи
# default_priority = 0                       # EN 300 392-2 табл. 14.46
# max_tx_seconds = 60
# link_timeout_ms = 6000
```

Далі єдине важливе правило: **групи диспетчера лише локальні.** Кожна група з `gssis` має лежати
в `local_ssi_ranges` у `[cell_info]`:

```toml
[cell_info]
local_ssi_ranges = [[0, 90]]     # TG 0–90 лишаються в цій соті; 91+ можуть іти в Brew
```

Якщо групу диспетчера можна маршрутизувати в Brew, станція все одно запускається, але канал
диспетчера — ні, і журнал каже, яка група конфліктує. Виклики диспетчера ніколи не потрапляють у Brew,
хоч би що просила платформа (три окремі перевірки, див. [dispatcher-link.md](dispatcher-link.md)).

Тепер `config.toml` містить токен станції: тримайте його поза git (`/config.toml` є в
`.gitignore`) і не публікуйте.

## 4. Запуск і перевірка

Перезапустіть станцію так, як ви її запускаєте, наприклад:

```bash
sudo systemctl restart bluestation-bs
sudo journalctl -u bluestation-bs -f -o cat
```

Очікувано в журналі:

```
 -> Dispatcher link enabled (core 192.168.1.10:4433)
Dispatcher: link to core is up
Dispatcher: sending state of 3 registered radio(s)
```

У консолі: *Станції* показують станцію **ONLINE** (час роботи, несучі, стан Brew, repeater
ID); *Радіостанції* — її зареєстровані радіостанції; *Групи* — групи, які вони слухають.

Далі: задайте ISSI диспетчера і запрограмуйте радіостанції (flowstation-platform `docs/uk/getting-started.md`,
кроки 3 і 5).

## 5. Зміна, вимкнення, повернення

| Завдання | Як |
|---|---|
| новий токен (старий витік, станцію замінено) | консоль *Базові станції → Новий токен*, впишіть його в `token`, перезапустіть |
| ядро переїхало | новий `server`; після нових сертифікатів ядра також новий `core-cert.pem` |
| вимкнути канал | `enabled = false` або приберіть секцію, перезапустіть — станція поводиться точно так, як без каналу форку |
| повернутися до попереднього бінарника | зупиніть станцію, поверніть `bluestation-bs.prev`, запустіть |

## 6. Усунення несправностей

| Рядок журналу | Значення, рішення |
|---|---|
| `Dispatcher link NOT started: gssis [..] are Brew-routable` | група диспетчера поза `local_ssi_ranges`; додайте її туди або приберіть з `gssis` |
| `Dispatcher link NOT started: … cannot read ca_cert …` | файлу сертифіката немає або користувач станції не може його прочитати — перевірте шлях і права |
| `Dispatcher: cannot reach core (…); retrying in …` | неправильний `server`, UDP 4433 заблоковано дорогою, платформа не працює; невідповідність сертифіката чи `server_name` теж закінчується тут — дивіться причину в дужках |
| `core refused this node: …` | неправильні `node_id` або `token`, токен замінено, станцію вимкнено чи видалено в консолі |
| `Dispatcher: link to core lost: …` | мережа або перезапуск платформи; станція перепідключається сама з паузами, передача диспетчера зупиняється протягом `link_timeout_ms` |
| `Dispatcher: settings not fully applied: …` | консоль надіслала те, що станція відхилила (напр. ISSI диспетчера, який уже використовує радіостанція тут); зберігається попереднє значення |
| `GSSI … became Brew-routable at runtime; refusing the dispatcher call` | конфігурацію змінено наживо, і група диспетчера більше не локальна |

Історія змін каналу: [dispatcher-changelog.md](dispatcher-changelog.md).
