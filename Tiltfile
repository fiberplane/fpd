# SSH Troubleshooting:
# - If you get the error "Build Failed: ImageBuild: could not parse ssh: [default]:...":
#   Make sure the ssh-agent is running BEFORE you run `tilt up` by running the command:
#     eval $(ssh-agent) && ssh-add
#   (you should run this in the same terminal session as you run `tilt up` in)

include('../relay/Tiltfile')

run_proxy_on_host = 'proxy' in os.getenv('RUN_ON_HOST', '').split(',') or os.getenv('RUN_ON_HOST') == 'all'
run_relay_on_host = 'relay' in os.getenv('RUN_ON_HOST', '').split(',') or os.getenv('RUN_ON_HOST') == 'all'

# Mapping from the provider name to the port it listens on
all_providers = {
  'elasticsearch': 9200,
  'loki': 3100,
  'prometheus': 9090,
}

# Run the data source providers
data_sources_yaml = ''
providers = []
if os.getenv('PROVIDERS'):
  if os.getenv('PROVIDERS') == 'all':
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

resource_deps = ['relay']
if len(providers) > 0:
  resource_deps.extend(providers)
if run_relay_on_host and run_proxy_on_host:
  fiberplane_endpoint = 'ws://localhost:3001'
elif run_relay_on_host:
  fiberplane_endpoint = 'ws://host.docker.internal:3001'
else:
  fiberplane_endpoint = 'ws://relay'
env={
  'RUST_LOG': 'proxy=trace',
  'LISTEN_ADDRESS': '127.0.0.1:3002',
  'FIBERPLANE_ENDPOINT': fiberplane_endpoint,
  'AUTH_TOKEN':'MVPpfxAYRxcQ4rFZUB7RRzirzwhR7htlkU3zcDm-pZk',
}

if os.getenv('LOCAL_PROXY'):
  # Write the data_sources.yaml to disk and point the proxy to it
  env['DATA_SOURCES'] = 'deployment/local/data_sources.yaml'
  local('echo %s > deployment/local/data_sources.yaml' % shlex.quote(data_sources_yaml))

  local_resource('proxy', 
    serve_env=env,
    serve_cmd='cargo run --bin proxy',
    dir='proxy',
    deps=['Cargo.toml', 'Cargo.lock', 'src', 'migrations'], 
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
