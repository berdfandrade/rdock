# rdock

Dock no estilo do macOS para X11 (Cinnamon, XFCE, MATE…), escrita em Rust puro: sem GTK nem Qt.
Usa x11rb para o X11, tiny-skia e resvg para desenhar e ab_glyph para o texto.

## O que faz

- **Magnificação olho de peixe**: os ícones crescem com curva cosseno conforme o mouse se aproxima, animados com molas.
- **Bounce ao abrir**: o ícone quica até a janela do app aparecer.
- **Indicador de execução**: um ponto sob cada app aberto. Apps abertos que não estão fixados aparecem depois de um separador.
- **Tooltip** com o nome do app.
- **Clique**: abre o app ou traz a janela dele para frente.
- **Menu do botão direito**: Abrir/Mostrar, Manter no Dock/Remover do Dock, Encerrar.
- **Arrastar para reordenar**, com os vizinhos abrindo espaço. Arrastar um ícone para fora da dock o remove com um "puf". Arrastar um app aberto para a área dos fixados o fixa.
- **Launchpad** (primeiro ícone): grade em tela cheia com **todos os apps instalados** (arquivos `.desktop` do padrão XDG, incluindo Flatpak e Snap). Tem fundo desfocado, zoom de entrada, páginas (roda do mouse, setas ou arrastar) e busca por teclado que ignora acentos. Enter abre o primeiro resultado e Esc fecha.
- **Lixeira**: o ícone muda quando há itens nela. O clique abre a lixeira.
- Ícones vêm do tema do sistema (ex.: Papirus).
- A dock reserva espaço na tela (`_NET_WM_STRUT`), para janelas maximizadas não ficarem por baixo dela.
- Parada, ela usa 0% de CPU: só redesenha enquanto alguma animação está rodando.

## Compilar e rodar

```bash
cargo build --release
./target/release/rdock
```

Requisitos: X11 com compositor (Cinnamon, XFCE com compositor ativo, picom…). Wayland não é suportado.

> Se você usa **Plank** ou outra dock, encerre-a antes (`pkill plank`). Ela fica por cima da rdock na parte de baixo da tela.

## Iniciar com o sistema

```bash
cargo install --path .
mkdir -p ~/.config/autostart
cat > ~/.config/autostart/rdock.desktop <<EOF
[Desktop Entry]
Type=Application
Name=rdock
Exec=$HOME/.cargo/bin/rdock
X-GNOME-Autostart-enabled=true
EOF
```

## Preferências

Abra pelo clique direito na prateleira, no separador, no Launchpad ou na Lixeira ("Preferências do Dock…"), pelo menu de aplicativos, ou com:

```bash
rdock --settings
```

Tudo é aplicado na hora: tamanho dos ícones, ampliação (liga/desliga e tamanho máximo), posição na tela (esquerda, embaixo, direita), ocultar a dock (nunca, sempre ou de forma inteligente — só quando a janela ativa encosta nela, como nos painéis do Cinnamon), bounce ao abrir, indicadores de apps abertos e tamanho do texto (100% a 180%). Ficam salvas em `~/.config/rdock/settings`.

Os apps fixados ficam em `~/.config/rdock/pinned`, com um ID `.desktop` por linha.
Na primeira execução, a lista é montada com o gerenciador de arquivos, o navegador, o terminal e os favoritos do Cinnamon.

## Temas

Um tema é **um PNG simples** com o desenho da prateleira na horizontal (como ela fica embaixo da tela).

- A imagem é escalada para a altura da prateleira.
- É dividida em 3 faixas verticais: as duas pontas (cada uma com **1/3 da altura** da imagem) ficam fixas, e o **meio é repetido** conforme a dock cresce. Mantenha os cantos arredondados dentro das pontas e faça o meio "ladrilhável".
- Na esquerda/direita da tela a imagem é girada automaticamente: a base do PNG fica sempre virada para a borda da tela.
- Transparência funciona (o canal alfa do PNG é usado).

Coloque o arquivo em `~/.config/rdock/themes/` (o nome do arquivo vira o nome do tema) e escolha em *Preferências → Tema*. O link "Abrir pasta de temas" abre essa pasta. Temas de exemplo (Grafite, Gelo, Neon, Pôr do sol, Madeira) ficam em [`themes/`](themes/); para instalá-los:

```bash
mkdir -p ~/.local/share/rdock/themes && cp themes/*.png ~/.local/share/rdock/themes/
```

O tema escolhido fica salvo em `~/.config/rdock/theme`. "Vidro" é o tema embutido.

## Estrutura

| Arquivo | Função |
|---|---|
| `main.rs` | Loop de eventos, estado compartilhado, associação janela → app |
| `dock.rs` | Layout, magnificação, bounce, arrastar, tooltip, menu, desenho |
| `launchpad.rs` | Grade de apps, busca, páginas, captura e desfoque do fundo |
| `apps.rs` | Leitura dos `.desktop`, busca e lançamento (via `gio launch`) |
| `icons.rs` | Busca no tema de ícones (spec XDG) e rasterização SVG/PNG |
| `draw.rs` | Primitivas, cache de ícones redimensionados, texto |
| `settings.rs` | Janela de preferências (sliders, chaves, seletor de posição) |
| `x.rs` | Janelas ARGB, upload de imagem, EWMH, teclado |
| `anim.rs` | Molas amortecidas |
