# Phase 2 — Marco 1: plano de implementação da geometria NIF estática

## Objetivo

Garantir que toda geometria estática alcançável por `STAT`, `MSTT` e `FURN` seja convertida para
GLB com a mesma posição, orientação, escala e hierarquia observadas no Skyrim. O marco termina com
zero falhas estruturais no conjunto alcançável e com evidência visual de que assets compostos não
estão desmontados, deitados ou girados no eixo errado.

Este plano detalha o Marco 1 de [`02-completion-plan.md`](02-completion-plan.md). Materiais, alpha,
texturas e fidelidade do terreno continuam no Marco 2 e no Marco 4, exceto quando forem necessários
para distinguir claramente uma falha geométrica de uma falha visual.

## Estado de partida

A Phase 1 já demonstrou leitura estrutural de 22.047 NIFs, conversão de 21.588 candidatos
renderizáveis, classificação de 459 arquivos não renderizáveis e zero falhas de parsing ou
conversão no corpus auditado. Isso comprova fechamento estrutural, mas não fidelidade espacial.

Os pontos técnicos que ainda impedem o Marco 1 são:

- `StaticMesh` guarda os buffers geométricos, mas não identifica o bloco de origem, o pai nem o
  transform local do shape;
- o exportador cria nós glTF estáticos com `translation`, `rotation` e `scale` vazios, fazendo cada
  shape composto aparecer na origem com transform identidade;
- posições de referências são convertidas de Creation Engine para Bevy por
  `(x, y, z) -> (x, z, -y)`, enquanto as rotações Euler são aplicadas diretamente nos eixos Bevy;
- alguns caminhos substituem valores não finitos por zero ou por uma normal padrão, mascarando o
  arquivo/bloco que deveria falhar de maneira diagnosticável;
- os bounds já percorrem a hierarquia glTF, mas só serão corretos quando o GLB preservar a
  hierarquia e todos os transforms relevantes.

## Decisões de implementação

### Um contrato único de coordenadas

Definir explicitamente a base `B` que leva vetores Creation/NIF para Bevy/glTF:

```text
B(x, y, z) = (x, z, -y)
```

Essa base é uma rotação própria, portanto não deve inverter winding. Posição e direção usam `B`;
uma rotação usa conjugação de base:

```text
R_bevy = B * R_creation * inverse(B)
```

Os ângulos do campo `DATA` de `REFR` giram em sentido horário em torno de cada eixo, a convenção
Gamebryo de matriz transposta: `R_creation` é o inverso da composição `Rz * Ry * Rx`, isto é,
`Rx(-x) * Ry(-y) * Rz(-z)`, e um yaw puro é uma direção medida em sentido horário a partir do norte.
A evidência: nas portas de carga do `Skyrim.esm` (2026-09-22), para 63 modelos de porta com 5 ou
mais colocações, 88% das portas põem o ponto de chegada `XTEL` no ângulo habitual do modelo sob esta
convenção, contra 42% sob a anti-horária usada até então. Fixtures canônicas fixam essa ordem. Não serão mantidas fórmulas independentes no conversor e no
runtime. O contrato e os vetores/matrizes de teste ficarão em um módulo pequeno de `shared`, usando
arrays para não acoplar as versões diferentes de `glam` usadas pelo parser NIF e pelo Bevy.

### Preservar a cena, sem assar transforms nos vértices

O GLB deve representar a árvore NIF. Nós `NiNode`/`BSFadeNode` que participam do caminho até um
shape serão emitidos com seus transforms locais; cada shape apontará para seu mesh. Uma raiz glTF
aplicará a mudança de base NIF Z-up para glTF Y-up uma única vez. Vértices e normais permanecem no
espaço local do shape, evitando perda de precisão e dupla aplicação de transforms.

Nós de controle sem geometria podem ser omitidos somente quando seu transform acumulado for
preservado no descendente. Referências inválidas, ciclos ou pais ambíguos devem falhar com o caminho
do arquivo e os índices/tipos dos blocos envolvidos.

### Falhar fechado

Geometria declarada não pode virar GLB vazio nem ter NaN substituído silenciosamente. Erros devem
informar arquivo, bloco, tipo e campo. O GLB é publicado somente depois de validação estrutural,
auditoria de bounds e escrita temporária bem-sucedidas.

## Pacotes de trabalho

### 1. Fixar baseline e casos de regressão

1. Executar `asset-closure` sobre `STAT`, `MSTT` e `FURN` e salvar fora do Git o relatório, hashes,
   load order, coordenadas e IDs das referências usadas na reprodução.
2. Selecionar fixtures mínimas para:
   - um `BSTriShape` simples;
   - um asset composto com pai rotacionado e filho transladado;
   - `BSDynamicTriShape` e `BSLODTriShape` alcançáveis;
   - `NiTriShape`/`NiTriShapeData` legado;
   - `NiTriStripsData`, apenas se aparecer no fechamento alcançável.
3. Registrar capturas antes da correção nas áreas rural e densa, sem versionar assets proprietários.
4. Acrescentar ao relatório de auditoria contagens por família geométrica, presença de hierarquia,
   profundidade máxima e motivos de exclusão.

**Saída:** baseline reproduzível e conjunto de regressão limitado ao conteúdo consumido pelo
runtime da Phase 2.

### 2. Implementar e testar o contrato de coordenadas

1. Criar o módulo de coordenadas em `crates/shared` com transformação de vetores, matrizes 3x3 e
   composição de base.
2. Representar os ângulos `REFR/DATA` como rotação Creation antes de convertê-los para quaternion
   Bevy; remover a aplicação direta dos mesmos ângulos nos eixos Bevy.
3. Definir a rotação da raiz glTF que converte NIF para Y-up e documentar onde a mudança ocorre para
   impedir dupla conversão.
4. Cobrir identidade, rotações de 90 graus em X/Y/Z, composição não comutativa, translação, escala
   uniforme e round-trip da base com tolerância numérica explícita.

**Saída:** posição, rotação e bounds usam a mesma convenção verificável no conversor e no engine.

### 3. Introduzir uma representação intermediária de cena estática

1. Substituir a lista plana de `StaticMesh` por uma IR que carregue:
   - índice e tipo do bloco NIF de origem;
   - nome estável;
   - índice opcional do pai e filhos ordenados;
   - transform local (`translation`, matriz/quaternion de rotação e escala);
   - geometria opcional e material associado.
2. Construir o grafo a partir dos roots e children do NIF, mantendo ordem determinística.
3. Validar referências de bloco, ciclos, múltiplos pais, transforms não finitos e escalas inválidas.
4. Permitir que nós sem mesh permaneçam na IR quando forem necessários para preservar transforms.

**Saída:** uma árvore validada e independente do formato de saída, comum às famílias estáticas.

### 4. Adaptar todas as famílias geométricas alcançáveis

1. Migrar `BSTriShape`, `BSDynamicTriShape`, `BSSubIndexTriShape`/`BSLODTriShape` e
   `NiTriShape`/`NiTriShapeData` para a IR comum.
2. Preservar posições, normais, tangentes disponíveis, UVs, cores, índices, material e transform.
3. Implementar `NiTriStripsData` somente se o baseline mostrar que ele é alcançável; converter
   strips em triângulos removendo degenerados e alternando winding de forma testada.
4. Validar cardinalidade de atributos, índices fora dos limites, degeneração acima do limite
   acordado, valores não finitos, AABB vazia e winding.
5. Remover substituições silenciosas de NaN/inf por zero ou vetores padrão.

**Saída:** todas as famílias no fechamento produzem a mesma IR validada ou um erro contextual.

### 5. Emitir GLB hierárquico e bounds finais

1. Emitir um nó glTF por nó relevante da IR e um mesh por payload geométrico, preservando a relação
   pai/filho e os transforms locais.
2. Inserir uma única raiz de mudança de base e manter os atributos no espaço local.
3. Manter associação determinística entre primitive e material sem antecipar o trabalho visual do
   Marco 2.
4. Validar o GLB em memória e calcular bounds percorrendo a cena completa antes da publicação.
5. Fazer a auditoria comparar bounds calculados dos vértices transformados com os bounds lidos do
   GLB, incluindo todos os oito cantos sob rotação e escala.

**Saída:** assets compostos conservam sua montagem, e os bounds representam a cena renderizada.

### 6. Corrigir placement e bounds no runtime

1. Trocar a criação direta de Euler em `crates/engine/src/streaming.rs` pela função de conversão de
   rotação do contrato compartilhado.
2. Garantir a composição na ordem: transform da referência no mundo, raiz de mudança de base do
   GLB e hierarquia local do NIF.
3. Manter escala `XSCL` uniforme e rejeitar valores não finitos antes de criar entidades.
4. Confirmar que `InstanceBounds`, frustum e futuro HZB usam o mesmo transform efetivamente
   renderizado.
5. Adicionar diagnóstico opt-in com FormID, caminho do modelo, posição/rotação Creation e matriz
   final Bevy para reproduções visuais sem poluir logs normais.

**Saída:** objetos do mundo têm orientação equivalente ao Skyrim e culling coerente com a imagem.

### 7. Invalidar artefatos e executar os gates

1. Incrementar o schema do conversor para invalidar GLBs produzidos pelo exportador plano e
   atualizar o contrato do launcher, engine e documentação.
2. Executar formatação, testes de todos os targets, Clippy com warnings negados e build release.
3. Executar a auditoria nos 22.047 NIFs e o `asset-closure` de `STAT`/`MSTT`/`FURN`.
4. Fazer uma conversão limpa e um rerun de cache, exigindo manifests completos em ambos.
5. Repetir os cenários rural e denso com capturas determinísticas e revisão lado a lado contra o
   Skyrim/Creation Kit no mesmo FormID e ângulo de câmera.
6. Executar rotação/escala da câmera e streaming rápido para detectar desaparecimento prematuro por
   bounds incorretos.

**Saída:** evidência automatizada e visual suficiente para encerrar o Marco 1.

## Estratégia de testes

### Unitários

- mudança de base e sua inversa;
- ordem Euler de `REFR/DATA` e conjugação de rotação;
- composição pai/filho com rotação, translação e escala;
- validação de NaN, índices, cardinalidade, ciclos e múltiplos pais;
- strips com winding alternado, degenerados e índices inválidos, se aplicável.

### Integração sintética

- NIF/IR com dois níveis de nós e dois shapes em transforms distintos;
- GLB com hierarquia esperada e bounds conhecidos analiticamente;
- referência do banco com rotação em cada eixo e bounds mundiais conhecidos;
- erro contextual sem panic e sem GLB parcial.

### Regressão com assets reais

- testes ignorados e habilitados por variáveis de ambiente para os hashes selecionados;
- auditoria integral sem copiar NIFs ou GLBs para o repositório;
- comparação visual de referências compostas, rochas, arquitetura e clutter nas áreas escolhidas.

## Critérios de conclusão do Marco 1

O marco pode ser marcado como concluído somente quando:

- `unsupported_geometry_files == 0` no fechamento alcançável;
- 100% dos GLBs alcançáveis passam validação estrutural e auditoria de bounds;
- nenhum dos 22.047 NIFs auditados causa panic;
- nenhum valor não finito é corrigido silenciosamente;
- fixtures canônicas provam posição, rotação e escala nos três eixos;
- capturas rural e densa não mostram peças desmontadas, assets deitados ou orientação divergente
  para as referências revisadas;
- rotação da câmera e streaming não causam desaparecimento prematuro por bounds;
- conversão limpa e rerun de cache terminam completos.

Problemas exclusivamente de material, alpha ou textura devem ser registrados para o Marco 2, sem
ser classificados como falha geométrica. O encerramento deste marco não encerra a Phase 2; HZB,
terreno, água, stress, estabilidade e a campanha final continuam nos marcos seguintes.

## Resultado da implementação (2026-09-10)

A implementação automatizada do Marco 1 está completa. O contrato de coordenadas agora é único
entre conversor e runtime; a cena estática preserva os transforms locais e a hierarquia
`NiNode`/`BSFadeNode`; shapes estáticos, shapes com payload em `NiSkinPartition` e geometria legada
passam pela mesma IR validada; e a publicação de GLB é atômica. O schema do conversor foi elevado
para 7 para invalidar os GLBs planos anteriores.

O fechamento foi corrigido para considerar somente modelos realmente instanciados por
`references -> statics`, em vez de todos os registros definidos. No load order local, ele contém
8.047 caminhos únicos: 8.038 possuem fonte distribuída e foram convertidos sem falha; nove apontam
para conteúdo não distribuído/editor-only e ficam classificados explicitamente como
`unavailable_source`. O relatório final marcou `geometry_passed: true`, com zero conversões
ausentes, zero GLBs geometricamente inválidos e um asset intencionalmente não renderizável.

A auditoria defensiva do corpus completo leu os 22.047 NIFs sem falha estrutural ou panic,
encontrou 175.943 nós de cena e profundidade máxima 27. As 16 falhas de conversão fora do
fechamento pertencem a efeitos/partículas e não bloqueiam a geometria estática da Phase 2.

O smoke test rural em build `release` encerrou com 240 frames, média de 266,67 FPS, P95 de
7,62 ms, crescimento de memória de 0,21 GiB e zero células com falha. A captura confirma que a
cena inicia e que o streaming usa os GLBs hierárquicos, mas a aprovação visual comparativa final
continua sendo uma revisão humana: texturas ausentes, materiais e alpha permanecem no Marco 2.

## Sequência recomendada de pull requests

1. **Contrato e testes de coordenadas:** módulo compartilhado e correção de `REFR`.
2. **IR e grafo NIF:** hierarquia validada, sem alterar ainda o formato publicado.
3. **Adaptadores geométricos:** famílias modernas e legadas, com validação estrita.
4. **Exportador GLB hierárquico:** nodes, transforms, raiz de base e bounds.
5. **Auditoria, schema e evidência:** invalidação de cache, corpus completo e capturas de aceite do
   Marco 1.

Cada PR deve manter `cargo test --workspace` e Clippy verdes. Mudanças de schema ficam na última PR
somente se as PRs anteriores não puderem publicar GLBs incorretos; caso qualquer etapa intermediária
altere outputs, o bump deve ocorrer na primeira dessas alterações.
