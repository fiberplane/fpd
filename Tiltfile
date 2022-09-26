# SSH Troubleshooting:
# - If you get the error "Build Failed: ImageBuild: could not parse ssh: [default]:...":
#   Make sure the ssh-agent is running BEFORE you run `tilt up` by running the command:
#     eval $(ssh-agent) && ssh-add
#   (you should run this in the same terminal session as you run `tilt up` in)

run_proxy_on_host = 'proxy' in os.getenv('RUN_ON_HOST', '').split(',') or os.getenv('RUN_ON_HOST') == 'all'

# Mapping from the provider name to the port it listens on
all_providers = {
  'elasticsearch': 9200,
  'loki': 3100,
  'prometheus': 9090,
}

# Run the data source providers
data_sources_yaml = ''
providers = []
if not os.getenv('PROVIDERS') or os.getenv('PROVIDERS') == 'all':
  providers = all_providers.keys()
else:
  providers = os.getenv('PROVIDERS').split(',')

for provider in providers:
  if provider not in all_providers:
    print('Provider %s not found. Available providers: %s' % (provider, ', '.join(all_providers.keys())))
    continue
  port = all_providers[provider]
  # If the proxy is running outside of docker, it will access the providers via ports on localhost
  # Otherwise, it will access them via the provider-specific kubernetes service
  url = 'http://{}:{}'.format('localhost' if run_proxy_on_host else provider, port)
  k8s_yaml([
    './deployment/local/%s_deployment.yaml' % provider,
    './deployment/local/%s_service.yaml' % provider,
  ])
  k8s_resource(provider, port_forwards=port, labels=['customer'])
  # Append the configuration to the data sources file used to configure the proxy
  data_sources_yaml += '''
{}:
  type: {}
  options:
    url: {}
'''.format(provider.capitalize(), provider, url)

# If elasticsearch is running, add fluentd to forward logs to it
if 'elasticsearch' in providers:
  fluentd_env={}
  # In some k8s versions, logging is done with a different standard
  # so this configures fluentd to understand the correct one
  # See https://github.com/fluent/fluentd-kubernetes-daemonset/tree/master#use-cri-parser-for-containerdcri-o-logs
  if k8s_context() in ['kind-kind']:
    fluentd_env['FLUENT_CONTAINER_TAIL_PARSER_TYPE'] = '/^(?<time>.+) (?<stream>stdout|stderr)( (?<logtag>.))? (?<log>.*)$/'

  k8s_yaml(local('./scripts/template.sh ./deployment/local/fluentd.template.yaml', env=fluentd_env))
  k8s_resource('fluentd',
    resource_deps=['elasticsearch'],
    objects=['fluentd:serviceaccount', 'fluentd:clusterrole', 'fluentd:clusterrolebinding'],
    labels=['customer'])

if len(providers) > 0:
  resource_deps.extend(providers)
if run_proxy_on_host:
  fiberplane_endpoint = 'ws://localhost:3000'
else:
  fiberplane_endpoint = 'ws://api'
env={
  'RUST_LOG': 'proxy=trace',
  'LISTEN_ADDRESS': '127.0.0.1:3002',
  'FIBERPLANE_ENDPOINT': fiberplane_endpoint,
  'AUTH_TOKEN':'MVPpfxAYRxcQ4rFZUB7RRzirzwhR7htlkU3zcDm-pZk',
}

if run_proxy_on_host:
  # Write the data_sources.yaml to disk and point the proxy to it
  env['DATA_SOURCES'] = 'deployment/local/data_sources.yaml'
  if os.getenv('OVERWRITE_DATA_SOURCES') != '0':
    local('echo %s > deployment/local/data_sources.yaml' % shlex.quote(data_sources_yaml))

  local_resource('proxy',
    serve_env=env,
    serve_cmd='cargo run --bin proxy',
    deps=['proxy', 'providers', 'deployment/local/data_sources.yaml'],
    resource_deps=resource_deps,
    # Note: this endpoint is called "/health" rather than "healthz"
    readiness_probe=probe(http_get=http_get_action(3002, path='/health')))
else:
  # Run docker with ssh option to access private git repositories
  docker_build('proxy:latest', '.', dockerfile='./Dockerfile.dev', ssh='default')
  k8s_resource(workload='proxy', resource_deps=resource_deps, objects=['proxy:configmap'], port_forwards=3002, labels=['customer'])

  k8s_yaml(local('./scripts/template.sh deployment/deployment.template.yaml', env=env))

  # Apply the data sources configuration using k8s configmap
  configmap = '''
  apiVersion: v1
  kind: ConfigMap
  metadata:
    name: proxy
    namespace: default
  data:
    data_sources.yaml: |
      %s
  ''' % data_sources_yaml.replace('\n', '\n      ')
  k8s_yaml(blob(configmap))
