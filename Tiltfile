# SSH Troubleshooting:
# - If you get the error "Build Failed: ImageBuild: could not parse ssh: [default]:...":
#   Make sure the ssh-agent is running BEFORE you run `tilt up` by running the command:
#     eval $(ssh-agent) && ssh-add
#   (you should run this in the same terminal session as you run `tilt up` in)

include('../relay/Tiltfile')

# Run the data source providers
data_sources_yaml = ''
providers = []
if os.getenv('PROVIDERS'):
  if os.getenv('PROVIDERS') == 'all':
    providers = ['elasticsearch', 'prometheus']
  else:
    providers = os.getenv('PROVIDERS').split(',')

# Elasticsearch
if 'elasticsearch' in providers:
  k8s_yaml([
    './deployment/local/elasticsearch_deployment.yaml',
    './deployment/local/elasticsearch_service.yaml',
  ])
  k8s_resource('elasticsearch', port_forwards=9200, labels=['customer'])
  elasticsearch_url = 'http://localhost:9200' if os.getenv('LOCAL_PROXY') else 'http://elasticsearch:9200'
  # Append the elasticsearch configuration to the data sources file
  data_sources_yaml += '''
Elasticsearch:
  type: elasticsearch
  options:
    url: %s
''' % elasticsearch_url

# Prometheus
if 'prometheus' in providers:
  k8s_yaml([
    './deployment/local/prometheus_deployment.yaml',
    './deployment/local/prometheus_service.yaml',
  ])
  k8s_resource('prometheus', port_forwards=9090, labels=['customer'])
  prometheus_url = 'http://localhost:9090' if os.getenv('LOCAL_PROXY') else 'http://prometheus:9090'
  # Append the prometheus configuration to the data sources file
  data_sources_yaml += '''
Prometheus:
  type: prometheus
  options:
    url: %s
''' % prometheus_url

resource_deps = ['relay']
if len(providers) > 0:
  resource_deps.extend(providers)
env={
  'RUST_LOG': 'proxy=trace',
  'LISTEN_ADDRESS': '127.0.0.1:3002',
  'FIBERPLANE_ENDPOINT': 'ws://localhost:3001' if os.getenv('LOCAL_PROXY') or os.getenv('LOCAL_RELAY') else 'ws://relay',
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
